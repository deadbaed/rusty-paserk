//! PKE (Public-Key Encryption).
//! PASERK uses Public-Key encryption to wrap symmetric keys for use in local tokens.
//!
//! <https://github.com/paseto-standard/paserk/blob/master/operations/PKE.md>

use std::{fmt, str::FromStr};

use base64::URL_SAFE_NO_PAD;
use blake2::{Blake2b, Blake2bMac, Digest};
use chacha20::XChaCha20;
use cipher::{inout::InOutBuf, KeyIvInit, StreamCipher};
use digest::Mac;
use generic_array::{
    sequence::{Concat, Split},
    typenum::{U24, U32},
    ArrayLength, GenericArray,
};
use rand::rngs::OsRng;
use rusty_paseto::core::PasetoError;
use sha2::Sha512;
use subtle::ConstantTimeEq;

// #[cfg(feature = "v3")]
// use rusty_paseto::core::V3;
#[cfg(feature = "v4")]
use rusty_paseto::core::V4;

use crate::key::{write_b64, Key, LocalKey, PublicKey, SecretKey, Version};

pub struct SealedKey<V: SealedVersion> {
    tag: GenericArray<u8, V::TagLen>,
    ephemeral_public_key: GenericArray<u8, V::EpkLen>,
    encrypted_data_key: GenericArray<u8, V::Local>,
}

impl<V: SealedVersion> Key<V, LocalKey> {
    pub fn seal(&self, sealing_key: &Key<V, PublicKey>) -> SealedKey<V> {
        V::seal(self, sealing_key)
    }
}

impl<V: SealedVersion> SealedKey<V> {
    pub fn unseal(
        self,
        unsealing_key: &Key<V, SecretKey>,
    ) -> Result<Key<V, LocalKey>, PasetoError> {
        V::unseal(self, unsealing_key)
    }
}

pub trait SealedVersion: Version + Sized {
    type TagLen: ArrayLength<u8>;
    type EpkLen: ArrayLength<u8>;

    type TotalLen: ArrayLength<u8>;
    #[allow(clippy::type_complexity)]
    fn split_total(total: GenericArray<u8, Self::TotalLen>) -> SealedKey<Self>;
    fn join_total(sealed: &SealedKey<Self>) -> GenericArray<u8, Self::TotalLen>;

    fn seal(
        plaintext_key: &Key<Self, LocalKey>,
        sealing_key: &Key<Self, PublicKey>,
    ) -> SealedKey<Self>;
    fn unseal(
        sealed_key: SealedKey<Self>,
        unsealing_key: &Key<Self, SecretKey>,
    ) -> Result<Key<Self, LocalKey>, PasetoError>;
}

impl SealedVersion for V4 {
    type TagLen = U32;
    type EpkLen = U32;

    type TotalLen = generic_array::typenum::U96;
    fn split_total(total: GenericArray<u8, Self::TotalLen>) -> SealedKey<Self> {
        let (tag, rest) = total.split();
        let (ephemeral_public_key, encrypted_data_key) = rest.split();
        SealedKey {
            tag,
            ephemeral_public_key,
            encrypted_data_key,
        }
    }
    fn join_total(sealed: &SealedKey<Self>) -> GenericArray<u8, Self::TotalLen> {
        sealed
            .tag
            .concat(sealed.ephemeral_public_key)
            .concat(sealed.encrypted_data_key)
    }

    fn seal(plaintext_key: &Key<V4, LocalKey>, sealing_key: &Key<V4, PublicKey>) -> SealedKey<V4> {
        // Given a plaintext data key (pdk), and an Ed25519 public key (pk).
        let pk = curve25519_dalek::edwards::CompressedEdwardsY::from_slice(sealing_key.as_ref())
            .unwrap();

        // step 1: Calculate the birationally-equivalent X25519 public key (xpk) from pk.
        // I wish the edwards point/montgomery point types were exposed by x/ed25519 libraries
        let xpk: x25519_dalek::PublicKey = pk.decompress().unwrap().to_montgomery().0.into();

        let esk = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
        let epk = x25519_dalek::PublicKey::from(&esk);

        let xk = esk.diffie_hellman(&xpk);

        let ek = Blake2b::<U32>::new()
            .chain_update([0x01])
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(xk.as_bytes())
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        let ak = Blake2b::<U32>::new()
            .chain_update([0x02])
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(xk.as_bytes())
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        let n = Blake2b::<U24>::new()
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        let mut edk = GenericArray::<u8, <V4 as Version>::Local>::default();
        XChaCha20::new(&ek, &n)
            .apply_keystream_inout(InOutBuf::new(plaintext_key.as_ref(), &mut edk).unwrap());

        let tag = Blake2bMac::<U32>::new_from_slice(&ak)
            .unwrap()
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(epk.as_bytes())
            .chain_update(edk)
            .finalize()
            .into_bytes();

        SealedKey {
            tag,
            ephemeral_public_key: epk.to_bytes().into(),
            encrypted_data_key: edk,
        }
    }

    fn unseal(
        mut sealed_key: SealedKey<Self>,
        unsealing_key: &Key<Self, SecretKey>,
    ) -> Result<Key<Self, LocalKey>, PasetoError> {
        let epk: [u8; 32] = sealed_key.ephemeral_public_key.into();
        let epk = x25519_dalek::PublicKey::from(epk);

        // expand sk
        let xsk = Sha512::default()
            .chain_update(&unsealing_key.as_ref()[..32])
            .finalize()[..32]
            .try_into()
            .unwrap();
        let xsk = curve25519_dalek::Scalar::from_bits_clamped(xsk);
        let xsk = x25519_dalek::StaticSecret::from(xsk.to_bytes());
        let xpk: x25519_dalek::PublicKey = (&xsk).into();

        let xk = xsk.diffie_hellman(&epk);

        let ak = Blake2b::<U32>::new()
            .chain_update([0x02])
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(xk.as_bytes())
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        let t2 = Blake2bMac::<U32>::new_from_slice(&ak)
            .unwrap()
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(epk.as_bytes())
            .chain_update(sealed_key.encrypted_data_key)
            .finalize()
            .into_bytes();

        // step 6: Compare t2 with t, using a constant-time compare function. If it does not match, abort.
        if sealed_key.tag.ct_ne(&t2).into() {
            return Err(PasetoError::InvalidSignature);
        }

        let ek = Blake2b::<U32>::new()
            .chain_update([0x01])
            .chain_update(V4::KEY_HEADER)
            .chain_update("seal.")
            .chain_update(xk.as_bytes())
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        let n = Blake2b::<U24>::new()
            .chain_update(epk.as_bytes())
            .chain_update(xpk.as_bytes())
            .finalize();

        XChaCha20::new(&ek, &n).apply_keystream(&mut sealed_key.encrypted_data_key);
        Ok(sealed_key.encrypted_data_key.into())
    }
}

impl<V: SealedVersion> FromStr for SealedKey<V> {
    type Err = PasetoError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s
            .strip_prefix(V::KEY_HEADER)
            .ok_or(PasetoError::WrongHeader)?;
        let s = s.strip_prefix("seal.").ok_or(PasetoError::WrongHeader)?;

        let mut total = GenericArray::<u8, V::TotalLen>::default();
        let len = base64::decode_config_slice(s, URL_SAFE_NO_PAD, &mut total)?;
        if len != 96 {
            return Err(PasetoError::PayloadBase64Decode {
                source: base64::DecodeError::InvalidLength,
            });
        }

        Ok(V::split_total(total))
    }
}

impl<V: SealedVersion> fmt::Display for SealedKey<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(V4::KEY_HEADER)?;
        f.write_str("seal.")?;

        write_b64(&V::join_total(self), f)
    }
}
