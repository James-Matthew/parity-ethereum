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

//! EIP-191 specific types
use serde::{Deserialize, Deserializer};
use serde::de;
use v1::types::{H160, Bytes};

pub enum EIP191Version {
	StructuredData,
	PersonalMessage,
	WithValidator
}

#[derive(Deserialize)]
pub struct WithValidator {
	// address of intended validator
	pub address: H160,
	// application specific data
	pub application_data: Bytes
}

impl<'de> Deserialize<'de> for EIP191Version {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
		where
			D: Deserializer<'de>,
	{
		let s = String::deserialize(deserializer)?;
		let byte_version = match s.as_str() {
			"0x00" => EIP191Version::WithValidator,
			"0x01" => EIP191Version::StructuredData,
			"0x45" => EIP191Version::PersonalMessage,
			other => return Err(de::Error::custom(format!("Invalid byte version '{}'", other))),
		};
		Ok(byte_version)
	}
}
