// Code adapted from: https://github.com/gakonst/ethers-rs/blob/master/ethers-core/src/types/signature.rs

use crate::utils::hash_message;
use alloc::{borrow::ToOwned, string::String, vec::Vec};
use codec::{Decode, Encode};
use core::convert::TryFrom;
use elliptic_curve::{consts::U32, sec1::ToEncodedPoint as _};
use frame_support::ensure;
use generic_array::GenericArray;
use k256::{
	ecdsa::{
		recoverable::{Id as RecoveryId, Signature as RecoverableSignature},
		Error as K256SignatureError, Signature as K256Signature,
	},
	PublicKey as K256PublicKey,
};
use scale_info::TypeInfo;
use sp_core::{H160, H256, U256};
use sp_runtime::{
	traits::{Hash, Keccak256},
	RuntimeDebug,
};
use thiserror_no_std::Error;

#[cfg(feature = "std")]
use core::{fmt, str::FromStr};
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

type Address = H160;

/// An error involving a signature.
#[derive(Debug, Error)]
pub enum SignatureError {
	/// Invalid length, secp256k1 signatures are 65 bytes
	#[error("invalid signature length, got {0}, expected 65")]
	InvalidLength(usize),
	/// When parsing a signature from string to hex
	#[error(transparent)]
	DecodingError(#[from] hex::FromHexError),
	/// Thrown when signature verification failed (i.e. when the address that
	/// produced the signature did not match the expected address)
	#[error("Signature verification failed. Expected {0:?}, got {1:?}")]
	VerificationError(Address, Address),
	/// Internal error during signature recovery
	#[error(transparent)]
	K256Error(#[from] K256SignatureError),
	/// Error in recovering public key from signature
	#[error("Public key recovery error")]
	RecoveryError,
}

/// Recovery message data.
///
/// The message data can either be a binary message that is first hashed
/// according to EIP-191 and then recovered based on the signature or a
/// precomputed hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryMessage {
	/// Message bytes
	Data(Vec<u8>),
	/// Message hash
	Hash(H256),
}

/// An ECDSA signature
#[derive(Clone, Encode, Decode, PartialEq, Eq, RuntimeDebug, TypeInfo)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Signature {
	/// R value
	pub r: U256,
	/// S Value
	pub s: U256,
	/// V value
	pub v: u64,
}

#[cfg(feature = "std")]
impl fmt::Display for Signature {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let sig = <[u8; 65]>::from(self);
		write!(f, "{}", hex::encode(&sig[..]))
	}
}

impl Signature {
	/// Verifies that signature on `message` was produced by `address`
	pub fn verify<M, A>(&self, message: M, address: A) -> Result<(), SignatureError>
	where
		M: Into<RecoveryMessage>,
		A: Into<Address>,
	{
		let address = address.into();
		let recovered = self.recover(message)?;
		ensure!(
			recovered == address,
			SignatureError::VerificationError(address, recovered)
		);

		Ok(())
	}

	/// Recovers the Ethereum address which was used to sign the given message.
	///
	/// Recovery signature data uses 'Electrum' notation, this means the `v`
	/// value is expected to be either `27` or `28`.
	pub fn recover<M>(&self, message: M) -> Result<Address, SignatureError>
	where
		M: Into<RecoveryMessage>,
	{
		let message = message.into();
		let message_hash = match message {
			RecoveryMessage::Data(ref message) => hash_message(message),
			RecoveryMessage::Hash(hash) => hash,
		};

		let (recoverable_sig, _recovery_id) = self.as_signature()?;
		let verify_key = recoverable_sig
			.recover_verifying_key_from_digest_bytes(message_hash.as_ref().into())?;

		let public_key = K256PublicKey::from(&verify_key);
		let public_key = public_key.to_encoded_point(/* compress = */ false);
		let public_key = public_key.as_bytes();
		debug_assert_eq!(public_key[0], 0x04);
		let hash = Keccak256::hash(&public_key[1..]);
		Ok(Address::from_slice(&hash[12..]))
	}

	/// Retrieves the recovery signature.
	fn as_signature(&self) -> Result<(RecoverableSignature, RecoveryId), SignatureError> {
		let recovery_id = self.recovery_id()?;
		let signature = {
			let mut r_bytes = [0u8; 32];
			let mut s_bytes = [0u8; 32];
			self.r.to_big_endian(&mut r_bytes);
			self.s.to_big_endian(&mut s_bytes);
			let gar: &GenericArray<u8, U32> = GenericArray::from_slice(&r_bytes);
			let gas: &GenericArray<u8, U32> = GenericArray::from_slice(&s_bytes);
			let sig = K256Signature::from_scalars(*gar, *gas)?;
			RecoverableSignature::new(&sig, recovery_id)?
		};

		Ok((signature, recovery_id))
	}

	/// Retrieve the recovery ID.
	pub fn recovery_id(&self) -> Result<RecoveryId, SignatureError> {
		let standard_v = normalize_recovery_id(self.v);
		Ok(RecoveryId::new(standard_v)?)
	}

	/// Copies and serializes `self` into a new `Vec` with the recovery id included
	#[allow(clippy::wrong_self_convention)]
	pub fn to_vec(&self) -> Vec<u8> {
		self.into()
	}
}

fn normalize_recovery_id(v: u64) -> u8 {
	match v {
		0 => 0,
		1 => 1,
		27 => 0,
		28 => 1,
		v if v >= 35 => ((v - 1) % 2) as _,
		_ => 4,
	}
}

impl From<sp_core::ecdsa::Signature> for Signature {
	fn from(src: sp_core::ecdsa::Signature) -> Self {
		let raw_src = src.0;
		let r = U256::from_big_endian(&raw_src[..32]);
		let s = U256::from_big_endian(&raw_src[32..64]);
		let v: u64 = raw_src[64].into();

		Self { r, s, v }
	}
}

impl<'a> TryFrom<&'a [u8]> for Signature {
	type Error = SignatureError;

	/// Parses a raw signature which is expected to be 65 bytes long where
	/// the first 32 bytes is the `r` value, the second 32 bytes the `s` value
	/// and the final byte is the `v` value in 'Electrum' notation.
	fn try_from(bytes: &'a [u8]) -> Result<Self, Self::Error> {
		if bytes.len() != 65 {
			return Err(SignatureError::InvalidLength(bytes.len()));
		}

		let v = bytes[64];
		let r = U256::from_big_endian(&bytes[0..32]);
		let s = U256::from_big_endian(&bytes[32..64]);

		Ok(Signature { r, s, v: v.into() })
	}
}

#[cfg(feature = "std")]
impl FromStr for Signature {
	type Err = SignatureError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		let s = s.strip_prefix("0x").unwrap_or(s);
		let bytes = hex::decode(s)?;
		Signature::try_from(&bytes[..])
	}
}

impl From<&Signature> for [u8; 65] {
	fn from(src: &Signature) -> [u8; 65] {
		let mut sig = [0u8; 65];
		src.r.to_big_endian(&mut sig[..32]);
		src.s.to_big_endian(&mut sig[32..64]);
		// TODO: What if we try to serialize a signature where
		// the `v` is not normalized?

		// The u64 to u8 cast is safe because `sig.v` can only ever be 27 or 28
		// here. Regarding EIP-155, the modification to `v` happens during tx
		// creation only _after_ the transaction is signed using
		// `ethers_signers::to_eip155_v`.
		sig[64] = src.v as u8;
		sig
	}
}

impl From<Signature> for [u8; 65] {
	fn from(src: Signature) -> [u8; 65] {
		<[u8; 65]>::from(&src)
	}
}

impl From<&Signature> for Vec<u8> {
	fn from(src: &Signature) -> Vec<u8> {
		<[u8; 65]>::from(src).to_vec()
	}
}

impl From<Signature> for Vec<u8> {
	fn from(src: Signature) -> Vec<u8> {
		<[u8; 65]>::from(&src).to_vec()
	}
}

impl From<&[u8]> for RecoveryMessage {
	fn from(s: &[u8]) -> Self {
		s.to_owned().into()
	}
}

impl From<Vec<u8>> for RecoveryMessage {
	fn from(s: Vec<u8>) -> Self {
		RecoveryMessage::Data(s)
	}
}

impl From<&str> for RecoveryMessage {
	fn from(s: &str) -> Self {
		s.as_bytes().to_owned().into()
	}
}

impl From<String> for RecoveryMessage {
	fn from(s: String) -> Self {
		RecoveryMessage::Data(s.into_bytes())
	}
}

impl From<[u8; 32]> for RecoveryMessage {
	fn from(hash: [u8; 32]) -> Self {
		H256(hash).into()
	}
}

impl From<H256> for RecoveryMessage {
	fn from(hash: H256) -> Self {
		RecoveryMessage::Hash(hash)
	}
}

#[cfg(feature = "std")]
// Want to convert ethers signature into our no-std version in tests
impl From<ethers_core::types::Signature> for Signature {
	fn from(sig: ethers_core::types::Signature) -> Self {
		// ethers-core 0.13.0 uses primitive types 0.11.1, sp-core 4.0.0-dev
		// uses primitive types 0.10.1
		let r_bytes: [u8; 32] = sig.r.into();
		let s_bytes: [u8; 32] = sig.s.into();

		Self {
			r: r_bytes.into(),
			s: s_bytes.into(),
			v: sig.v,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn recover_web3_signature() {
		// test vector taken from:
		// https://web3js.readthedocs.io/en/v1.2.2/web3-eth-accounts.html#sign
		let signature = Signature::from_str(
            "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c"
        ).expect("could not parse signature");
		assert_eq!(
			signature.recover("Some data").unwrap(),
			Address::from_str("2c7536E3605D9C16a7a3D7b1898e529396a65c23").unwrap()
		);
	}

	#[test]
	fn signature_from_str() {
		let s1 = Signature::from_str(
            "0xaa231fbe0ed2b5418e6ba7c19bee2522852955ec50996c02a2fe3e71d30ddaf1645baf4823fea7cb4fcc7150842493847cfb6a6d63ab93e8ee928ee3f61f503500"
        ).expect("could not parse 0x-prefixed signature");

		let s2 = Signature::from_str(
            "aa231fbe0ed2b5418e6ba7c19bee2522852955ec50996c02a2fe3e71d30ddaf1645baf4823fea7cb4fcc7150842493847cfb6a6d63ab93e8ee928ee3f61f503500"
        ).expect("could not parse non-prefixed signature");

		assert_eq!(s1, s2);
	}
}
