// Copyright 2015-2018 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use byteorder::{ByteOrder, LittleEndian};
use compute::{FNV_PRIME, calculate_dag_item};
use keccak::H256;
use shared::{ETHASH_ACCESSES, ETHASH_EPOCH_LENGTH, ETHASH_MIX_BYTES, Node};

const PROGPOW_LANES: usize = 32;
const PROGPOW_REGS: usize = 16;
const PROGPOW_CACHE_WORDS: usize = 4 * 1024;
const PROGPOW_CNT_MEM: usize = ETHASH_ACCESSES;
const PROGPOW_CNT_CACHE: usize = 8;
const PROGPOW_CNT_MATH: usize = 8;
const PROGPOW_MIX_BYTES: usize = 2 * ETHASH_MIX_BYTES;

const FNV_HASH: u32 = 0x811c9dc5;

// TODO: rewrite without side effect
fn fnv1a_hash(h: &mut u32, d: u32) -> u32 {
    *h = (*h ^ d).wrapping_mul(FNV_PRIME);
	*h
}

struct Kiss99 {
	z: u32,
	w: u32,
	jsr: u32,
	jcong: u32,
}

impl Kiss99 {
	fn new(z: u32, w: u32, jsr: u32, jcong: u32) -> Kiss99 {
		Kiss99 { z, w, jsr, jcong }
	}

	fn next_u32(&mut self) -> u32 {
		self.z = 36969u32.wrapping_mul(self.z & 65535u32).wrapping_add(self.z >> 16);
		self.w = 18000u32.wrapping_mul(self.w & 65535u32).wrapping_add(self.w >> 16);
		let mwc = (self.z << 16).wrapping_add(self.w);
		self.jsr ^= self.jsr << 17;
		self.jsr ^= self.jsr >> 13;
		self.jsr ^= self.jsr << 5;
		self.jcong = 69069u32.wrapping_mul(self.jcong).wrapping_add(1234567u32);

		(mwc ^ self.jcong).wrapping_add(self.jsr)
	}
}

fn fill_mix(seed: u64, lane_id: u32) -> [u32; PROGPOW_REGS] {
    // Use FNV to expand the per-warp seed to per-lane
    // Use KISS to expand the per-lane seed to fill mix
	let mut fnv_hash = FNV_HASH;
    let mut rnd = Kiss99::new(
		fnv1a_hash(&mut fnv_hash, seed as u32),
		fnv1a_hash(&mut fnv_hash, (seed >> 32) as u32),
		fnv1a_hash(&mut fnv_hash, lane_id),
		fnv1a_hash(&mut fnv_hash, lane_id),
	);

	let mut mix = [0; PROGPOW_REGS];
    for i in 0..mix.len() {
        mix[i] = rnd.next_u32();
    }

	mix
}

fn progpow_init(seed: u64) -> (Kiss99, [u32; PROGPOW_REGS]) {
	let mut fnv_hash = FNV_HASH;
    let mut rnd = Kiss99::new(
		fnv1a_hash(&mut fnv_hash, seed as u32),
		fnv1a_hash(&mut fnv_hash, (seed >> 32) as u32),
		fnv1a_hash(&mut fnv_hash, seed as u32),
		fnv1a_hash(&mut fnv_hash, (seed >> 32) as u32),
	);
    // Create a random sequence of mix destinations for merge()
    // guaranteeing every location is touched once
    // Uses Fisher–Yates shuffle
	let mut mix_seq = [0u32; PROGPOW_REGS];
    for i in 0..mix_seq.len() {
        mix_seq[i] = i as u32;
    }
    for i in (0..mix_seq.len()).rev() {
        let j = rnd.next_u32() as usize % (i + 1);
		mix_seq.swap(i, j);
    }

    (rnd, mix_seq)
}

// Merge new data from b into the value in a
// Assuming A has high entropy only do ops that retain entropy
// even if B is low entropy
// (IE don't do A&B)
fn merge(a: &mut u32, b: u32, r: u32) {
    match r % 4 {
		0 => *a = a.wrapping_mul(33u32).wrapping_add(b),
		1 => *a = (*a ^ b).wrapping_mul(33u32),
		2 => *a = a.rotate_left((r >> 16) % 32) ^ b,
		3 => *a = a.rotate_right((r >> 16) % 32) ^ b,
		_ => unreachable!(),
    }
}

fn math(a: u32, b: u32, r: u32) -> u32 {
	match r % 11 {
		0 => a.wrapping_add(b),
		1 => a.wrapping_mul(b),
		2 => ((a as u64).wrapping_mul(b as u64) >> 32) as u32,
		3 => a.min(b),
		4 => a.rotate_left(b),
		5 => a.rotate_right(b),
		6 => a & b,
		7 => a | b,
		8 => a ^ b,
		9 => a.leading_zeros() + b.leading_zeros(),
		10 => a.count_ones() + b.count_ones(),
		_ => unreachable!(),
	}
}

fn progpow_loop<F>(
	seed: u64,
	loopp: usize,
	mix: &mut [[u32; PROGPOW_REGS]; PROGPOW_LANES],
	c_dag: &mut [u32; PROGPOW_CACHE_WORDS],
	lookup: F,
	data_size: usize,
) where F: Fn(usize) -> u32 {
    let offset_g = mix[loopp % PROGPOW_LANES][0] as usize % data_size;
	let offset_g = offset_g * PROGPOW_LANES;

	for l in 0..mix.len() {
		// global load to sequential locations
		// let data64 = lookup(2*(offset_g + l)); // FIXME: is this correct?
		let data64 = (lookup(2 * (offset_g + l) + 1) as u64) << 32 | (lookup(2 * (offset_g + l)) as u64);
		// initialize the seed and mix destination sequence
		let (mut rnd, mut mix_seq) = progpow_init(seed);
		let mut mix_seq_cnt = 0;

		let mix_src = |rnd: &mut Kiss99| rnd.next_u32() as usize % PROGPOW_REGS;
		let mut mix_dst = || {
			let ret = mix_seq[mix_seq_cnt % PROGPOW_REGS];
			mix_seq_cnt += 1;
			ret as usize
		};

		for i in 0..(PROGPOW_CNT_CACHE.max(PROGPOW_CNT_MATH)) {
			if i < PROGPOW_CNT_CACHE {
                // Cached memory access
                // lanes access random location
                let offset = mix[l][mix_src(&mut rnd)] as usize % PROGPOW_CACHE_WORDS;
                let data32 = c_dag[offset];
                merge(&mut mix[l][mix_dst()], data32, rnd.next_u32());
            }
            if i < PROGPOW_CNT_MATH {
                // Random Math
                let data32 = math(mix[l][mix_src(&mut rnd)], mix[l][mix_src(&mut rnd)], rnd.next_u32());
                merge(&mut mix[l][mix_dst()], data32, rnd.next_u32());
            }
		}

		// Consume the global load data at the very end of the loop
        // Allows full latency hiding
        merge(&mut mix[l][0], data64 as u32, rnd.next_u32());
        merge(&mut mix[l][mix_dst()], (data64 >> 32) as u32, rnd.next_u32());
	}
}

const KECCAKF_RNDC: [u32; 24] = [
	0x00000001, 0x00008082, 0x0000808a, 0x80008000, 0x0000808b, 0x80000001,
	0x80008081, 0x00008009, 0x0000008a, 0x00000088, 0x80008009, 0x8000000a,
	0x8000808b, 0x0000008b, 0x00008089, 0x00008003, 0x00008002, 0x00000080,
	0x0000800a, 0x8000000a, 0x80008081, 0x00008080, 0x80000001, 0x80008008
];

fn keccak_f800_round(st: &mut [u32; 25], r: usize) {
	let keccakf_rotc: [u32; 24] = [
		1,  3,  6,  10, 15, 21, 28, 36, 45, 55, 2,  14,
		27, 41, 56, 8,  25, 43, 62, 18, 39, 61, 20, 44
	];
	let keccakf_piln: [u32; 24] = [
		10, 7,  11, 17, 18, 3, 5,  16, 8,  21, 24, 4,
		15, 23, 19, 13, 12, 2, 20, 14, 22, 9,  6,  1
	];

	/* Theta*/
	let mut bc = [0u32; 5];
	for i in 0..bc.len() {
		bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
	}

	for i in 0..bc.len() {
		let t = bc[(i + 4) % 5] ^ bc[(i + 1) % 5].rotate_left(1);
		for j in (0..st.len()).step_by(5) {
			st[j + i] ^= t;
		}
	}

	/*Rho Pi*/
	let mut t = st[1];
	for i in 0..keccakf_rotc.len() {
		let j = keccakf_piln[i] as usize;
		bc[0] = st[j];
		st[j] = t.rotate_left(keccakf_rotc[i]);
		t = bc[0];
	}

	/* Chi*/
	for j in (0..st.len()).step_by(5) {
		for i in 0..bc.len() {
			bc[i] = st[j + i];
		}
		for i in 0..bc.len() {
			st[j + i] ^= (!bc[(i + 1) % 5]) & bc[(i + 2) % 5];
		}
	}

	/* Iota*/
	st[0] ^= KECCAKF_RNDC[r];
}

fn keccak_f800_short(header_hash: H256, nonce: u64, result: [u32; 8]) -> u64 {
	let mut st = [0u32; 25];

	for i in 0..8 {
		st[i] = (header_hash[4 * i] as u32) +
			((header_hash[4 * i + 1] as u32) << 8) +
			((header_hash[4 * i + 2] as u32) << 16) +
			((header_hash[4 * i + 3] as u32) << 24);
	}

	st[8] = nonce as u32;
	st[9] = (nonce >> 32) as u32;

	for i in 0..4 { // FIXME: check this
		st[10 + i] = result[i];
	}

	for r in 0..21 {
		keccak_f800_round(&mut st, r);
	}
	keccak_f800_round(&mut st, 21);

	(st[0] as u64) << 32 | st[1] as u64
}

fn keccak_f800_long(header_hash: H256, nonce: u64, result: [u32; 8]) -> H256 {
	let mut st = [0u32; 25];

	for i in 0..8 {
		st[i] = (header_hash[4 * i] as u32) +
			((header_hash[4 * i + 1] as u32) << 8) +
			((header_hash[4 * i + 2] as u32) << 16) +
			((header_hash[4 * i + 3] as u32) << 24);
	}

	st[8] = nonce as u32;
	st[9] = (nonce >> 32) as u32;

	for i in 0..4 { // FIXME: check this
		st[10 + i] = result[i];
	}

	for r in 0..21 {
		keccak_f800_round(&mut st, r);
	}
	keccak_f800_round(&mut st, 21);

	let res: [u32; 8] = [st[0], st[1], st[2], st[3], st[4], st[5], st[6], st[7]];
	// transmute to little endian bytes
	unsafe { ::std::mem::transmute(res) }
}

fn progpow_light(
	header_hash: H256,
	nonce: u64,
	size: u64,
	block_number: u64,
	cache: &[Node],
) -> (H256, H256) {
	let mut mix = [[0u32; PROGPOW_REGS]; PROGPOW_LANES];
	let mut lane_results = [0u32; PROGPOW_LANES];

	let mut c_dag = [0u32; PROGPOW_CACHE_WORDS];
	let mut result = [0u32; 8];

	let lookup = |index: usize| {
		let item = calculate_dag_item((index / 16) as u32, cache);
		LittleEndian::read_u32(&item.as_bytes()[(index % 16) * 4..])
	};

	for i in (0..PROGPOW_CACHE_WORDS).step_by(2) {
		c_dag[i] = lookup(2 * i);
		c_dag[i + 1] = lookup(2 * i + 1);
	}

	// initialize mix for all lanes
	let seed = keccak_f800_short(header_hash, nonce, result);
	for l in 0..mix.len() {
		mix[l] = fill_mix(seed, l as u32);
	}
	// execute the randomly generated inner loop
	let block_number_rounded = (block_number / ETHASH_EPOCH_LENGTH) * ETHASH_EPOCH_LENGTH;
	for i in 0..PROGPOW_CNT_MEM {
        progpow_loop(
			block_number_rounded,
			i,
			&mut mix,
			&mut c_dag,
			lookup,
			size as usize / PROGPOW_MIX_BYTES,
		);
	}
    // Reduce mix data to a single per-lane result
    for l in 0..lane_results.len() {
        lane_results[l] = FNV_HASH;
        for i in 0..PROGPOW_REGS {
            fnv1a_hash(&mut lane_results[l], mix[l][i]);
		}
	}

    // Reduce all lanes to a single 128-bit result
	result = [FNV_HASH; 8];
	for l in 0..PROGPOW_LANES {
        fnv1a_hash(&mut result[l % 8], lane_results[l]);
	}

	let digest = keccak_f800_long(header_hash, seed, result);
	// transmute to little endian bytes
	let result = unsafe { ::std::mem::transmute(result) };

	(digest, result)
}

#[cfg(test)]
mod test {
	use cache::{NodeCacheBuilder, OptimizeFor};
	use ethereum_types::H256;
	use shared::get_data_size;
	use std::env;
	use super::progpow_light;

	#[test]
	fn it_works() {
		struct ProgPowTest {
			block_number: u64,
			nonce: u64,
			header_hash: H256,
			digest: H256,
			result: H256,
		}

		let tests: &[ProgPowTest] = &[
			ProgPowTest {
				block_number: 568971,
				nonce: 2698189332257848714,
				header_hash: "0x000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f".into(),
				digest: "0xfb8dc4fa5ec9df003efea48e54d6e6b9ad14febf76461fcc17abf547c623c671".into(),
				result: "0x4b9f8eea14bc2b7e60b128199a9b3a39cf531cdac098708a74a34dce7e155dfc".into(),
			},
		];

		for test in tests {
			let builder = NodeCacheBuilder::new(OptimizeFor::Memory);
			let cache = builder.new_cache(env::temp_dir(), test.block_number);
			let size = get_data_size(test.block_number) as u64;

			let (digest, result) = progpow_light(
				test.header_hash.0,
				test.nonce,
				size,
				test.block_number,
				cache.as_ref(),
			);

			assert_eq!(digest, test.digest.0);
			assert_eq!(result, test.result.0);
		}
	}
}
