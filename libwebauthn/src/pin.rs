use super::transport::error::Error;

use aes::cipher::{block_padding::NoPadding, BlockDecryptMut};
use async_trait::async_trait;
use cbc::cipher::{BlockEncryptMut, KeyIvInit};
use hmac::Mac;
use p256::{
    ecdh::EphemeralSecret, elliptic_curve::sec1::FromEncodedPoint, EncodedPoint,
    PublicKey as P256PublicKey,
};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use tracing::{error, info, instrument};
use x509_parser::nom::AsBytes;

use crate::proto::{ctap2::Ctap2PinUvAuthProtocol, CtapError};

type Aes256CbcEncryptor = cbc::Encryptor<aes::Aes256>;
type Aes256CbcDecryptor = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = hmac::Hmac<Sha256>;

pub struct PinUvAuthToken {
    pub rpid: Option<String>,
    pub user_verified: bool,
    pub user_present: bool,
}

impl Default for PinUvAuthToken {
    fn default() -> Self {
        Self {
            rpid: None,
            user_verified: false,
            user_present: false,
        }
    }
}

#[async_trait]
pub trait PinProvider {
    async fn provide_pin(&self, attempts_left: Option<u32>) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct StaticPinProvider {
    pin: String,
}

impl StaticPinProvider {
    pub fn new(pin: &str) -> Self {
        Self {
            pin: pin.to_owned(),
        }
    }
}

#[async_trait]
impl PinProvider for StaticPinProvider {
    async fn provide_pin(&self, attempts_left: Option<u32>) -> Option<String> {
        info!(
            "Providing static PIN '{}' ({:?} attempts left)",
            self.pin, attempts_left
        );
        Some(self.pin.clone())
    }
}

pub trait PinUvAuthProtocol {
    fn version(&self) -> Ctap2PinUvAuthProtocol;

    /// encapsulate(peerCoseKey) → (coseKey, sharedSecret) | error
    ///   Generates an encapsulation for the authenticator’s public key and returns the message to transmit and the
    ///   shared secret.
    fn encapsulate(
        &self,
        peer_public_key: &cosey::PublicKey,
    ) -> Result<(cosey::PublicKey, Vec<u8>), Error>;

    // encrypt(key, demPlaintext) → ciphertext
    //   Encrypts a plaintext to produce a ciphertext, which may be longer than the plaintext.
    //   The plaintext is restricted to being a multiple of the AES block size (16 bytes) in length.
    fn encrypt(&self, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, Error>;

    // decrypt(key, ciphertext) → plaintext | error
    //   Decrypts a ciphertext and returns the plaintext.
    fn decrypt(&self, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error>;

    // authenticate(key, message) → signature
    //   Computes a MAC of the given message.
    fn authenticate(&self, key: &[u8], message: &[u8]) -> Vec<u8>;
}

pub struct PinUvAuthProtocolOne {
    private_key: EphemeralSecret,
    public_key: P256PublicKey,
}

impl PinUvAuthProtocolOne {
    pub fn new() -> Self {
        let private_key = EphemeralSecret::random(&mut OsRng);
        let public_key = private_key.public_key();
        Self {
            private_key,
            public_key,
        }
    }

    /// ecdh(peerCoseKey) → sharedSecret | error
    fn ecdh(&self, peer_public_key: &cosey::PublicKey) -> Result<Vec<u8>, Error> {
        // Parse peerCoseKey as specified for getPublicKey, below, and produce a P-256 point, Y.
        // If unsuccessful, or if the resulting point is not on the curve, return error.
        let cosey::PublicKey::EcdhEsHkdf256Key(peer_public_key) = peer_public_key else {
            error!(?peer_public_key, "Unsupported peerCoseKey format. Only EcdhEsHkdf256Key is supported.");
            return Err(Error::Ctap(CtapError::Other));
        };
        let encoded_point = EncodedPoint::from_affine_coordinates(
            peer_public_key.x.as_bytes().into(),
            peer_public_key.y.as_bytes().into(),
            false,
        );
        let Some(peer_public_key) = P256PublicKey::from_encoded_point(&encoded_point).into() else {
            error!("Failed to parse public key.");
            return Err(Error::Ctap(CtapError::Other));
        };

        // Calculate xY, the shared point. (I.e. the scalar-multiplication of the peer’s point, Y, with the
        // local private key agreement key.)
        let shared = self.private_key.diffie_hellman(&peer_public_key);

        // Return kdf(Z).
        Ok(self.kdf(shared.as_bytes().as_bytes()))
    }

    /// kdf(Z) → sharedSecret
    fn kdf(&self, bytes: &[u8]) -> Vec<u8> {
        let mut hasher = Sha256::default();
        hasher.update(bytes);
        hasher.finalize().to_vec()
    }

    /// getPublicKey()
    fn get_public_key(&self) -> cosey::PublicKey {
        let point = EncodedPoint::from(self.public_key);
        let x: heapless::Vec<u8, 32> =
            heapless::Vec::from_slice(point.x().expect("Not the identity point").as_bytes())
                .unwrap();
        let y: heapless::Vec<u8, 32> =
            heapless::Vec::from_slice(point.y().expect("Not identity nor compressed").as_bytes())
                .unwrap();
        cosey::PublicKey::P256Key(cosey::P256PublicKey {
            x: x.into(),
            y: y.into(),
        })
    }
}

impl PinUvAuthProtocol for PinUvAuthProtocolOne {
    fn version(&self) -> Ctap2PinUvAuthProtocol {
        Ctap2PinUvAuthProtocol::One
    }

    #[instrument(skip_all)]
    fn encapsulate(
        &self,
        peer_public_key: &cosey::PublicKey,
    ) -> Result<(cosey::PublicKey, Vec<u8>), Error> {
        // Let sharedSecret be the result of calling ecdh(peerCoseKey). Return any resulting error.
        let shared_secret = self.ecdh(peer_public_key)?;

        // Return(getPublicKey(), sharedSecret)
        Ok((self.get_public_key(), shared_secret))
    }

    fn encrypt(&self, key: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, Error> {
        // Return the AES-256-CBC encryption of demPlaintext using an all-zero IV.
        // (No padding is performed as the size of demPlaintext is required to be a multiple of the AES block length.)
        let iv: &[u8] = &[0; 16];
        let Ok(enc) = Aes256CbcEncryptor::new_from_slices(key, iv) else {
            error!(?key, "Invalid key for AES-256 encryption");
            return Err(Error::Ctap(CtapError::Other));
        };
        Ok(enc.encrypt_padded_vec_mut::<NoPadding>(plaintext))
    }

    fn authenticate(&self, key: &[u8], message: &[u8]) -> Vec<u8> {
        // Return the first 16 bytes of the result of computing HMAC-SHA-256 with the given key and message.
        let hmac = hmac_sha256(key, message);
        Vec::from(&hmac[..16])
    }

    fn decrypt(&self, key: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, Error> {
        // If the size of demCiphertext is not a multiple of the AES block length, return error.
        // Otherwise return the AES-256-CBC decryption of demCiphertext using an all-zero IV.
        if ciphertext.len() % 16 != 0 {
            error!(
                ?ciphertext,
                "Ciphertext length is not a multiple of AES block length"
            );
            return Err(Error::Ctap(CtapError::Other));
        }

        let iv: &[u8] = &[0; 16];
        let Ok(dec) = Aes256CbcDecryptor::new_from_slices(key, iv) else {
            error!(?key, "Invalid key for AES-256 decryption");
            return Err(Error::Ctap(CtapError::Other));
        };
        let Ok(plaintext) = dec.decrypt_padded_vec_mut::<NoPadding>(ciphertext) else {
            error!("Unpad error while decrypting");
            return Err(Error::Ctap(CtapError::Other));
        };
        Ok(plaintext)
    }
}

/// hash(pin) -> LEFT(SHA-256(pin), 16)
pub fn pin_hash(pin: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::default();
    hasher.update(pin);
    let hashed = hasher.finalize().to_vec();
    Vec::from(&hashed[..16])
}

pub fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut hmac = HmacSha256::new_from_slice(key).expect("Any key size is valid");
    hmac.update(message);
    hmac.finalize().into_bytes().to_vec()
}