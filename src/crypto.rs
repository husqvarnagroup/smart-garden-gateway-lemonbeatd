// SPDX-FileCopyrightText: GARDENA GmbH
//
// SPDX-License-Identifier: GPL-3.0-or-later

//! Implements lemonbeat cryptography
//!
//! # Cryptography
//! Since the spec doesn't do a good job explaining it, this section aims to do
//! that instead.
//!
//! ## Inclusion message format
//! - controller key (AES128-ECB-NOPAD), 16 bytes
//! - network-key encrypted using the controller-key, 16 bytes
//! - CRC16_XMODEM(controller_key | encrypted_network_key)
//!
//! ## Encryption
//! The above inclusion message is then encrypted using the the public-half of
//! the target devices' RSA key.  
//! For Gardena, all devices use the same RSA keypair also known as
//! `default key`. That key is **NOT** Gardena-specific and can be found in the
//! official lemonbeat stack source code.  
//! The inclusion samples in the lemonbeat specification don't contain the
//! private key but do have the matching public key in them.
//!
//! ## Transport
//! The encrypted inclusion message is sent to the device using a
//! `network_include` LSDL message.
//!
//! ## Network key
//! Knowledge about the network key is not required by any application
//! (like lemonbeatd) because encryption/decryption is done by the
//! lemonbeat-stack which in case of the gateway lives on the radio module.
//!
//! That being said, the key is an AES128 CCM key.  
//! That information can be found in section `2.2.2 Security` of the Lemonbeat
//! specification version 1.13 15/04/2020.

// CLEAN: The code in this file is quite messy and uses unnecessary
//       abstractions. That is especially the case for json decoding.

use crate::storage;

use anyhow::Context as _;
use block_modes::BlockMode as _;
use byteorder::ByteOrder as _;
use rand::Rng as _;
use rsa::Pkcs1v15Encrypt;

type Aes128EcbNopad = block_modes::Ecb<aes::Aes128, block_modes::block_padding::NoPadding>;

static CRC: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_XMODEM);
static DEFAULT_PUBLIC_KEY: &[u8; 64] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/data/public.key"));

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    FromHex(#[from] hex::FromHexError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    SerdeJson(#[from] serde_json::Error),
    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

/// A lemonbeat key in it's raw form
type Key = [u8; 16];

/// Generate a new lemonbeat key
fn generate_key() -> Key {
    let mut rng = rand::thread_rng();
    let mut key = [0u8; 16];
    rng.fill(&mut key);
    key
}

#[derive(serde::Deserialize)]
struct NetworkKeyRaw {
    network_key: String,
}

#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct NetworkKey {
    #[serde(
        serialize_with = "hex::serde::serialize",
        deserialize_with = "hex::serde::deserialize"
    )]
    network_key: Key,
}

impl NetworkKey {
    /// Generate a new network key using [rand::thread_rng]
    pub fn generate() -> Self {
        Self {
            network_key: generate_key(),
        }
    }

    pub async fn to_file<P: AsRef<std::path::Path>>(&self, path: P) -> Result<(), Error> {
        storage::save_json(path, self)
            .await
            .context("can't save network key")?;
        Ok(())
    }

    pub fn from_file<P: AsRef<std::path::Path> + Copy>(path: P) -> Result<Self, Error> {
        let file = std::fs::File::open(path.as_ref())?;

        // We don't know how this could've happened but there's nothing we can
        // do to fix it - not even manually by a human. So let's just delete it
        // and generate a new one.
        let metadata = file.metadata().context("can't get metadata")?;
        if metadata.len() == 0 {
            tracing::warn!("Empty network key file, delete");
            std::fs::remove_file(path).context("can't delete empty file")?;
            return Err(Error::Io(std::io::ErrorKind::NotFound.into()));
        }

        let reader = std::io::BufReader::new(file);
        let raw: NetworkKeyRaw =
            serde_json::from_reader(reader).context("failed to read network key from file")?;

        if raw.network_key.chars().any(|c| c.is_ascii_uppercase()) {
            let backup_path = path.as_ref().with_file_name("Network_key.json.upper_case");

            if std::path::Path::exists(backup_path.as_ref()) {
                tracing::error!("Upper-case network key reappeared. Generating fresh key.");
            } else {
                tracing::warn!(
                    "Upper-case network key (SGSE-1024). Rename and generating fresh key."
                );

                std::fs::rename(path, backup_path)
                    .context("can't rename uppercase network key file")?;
            }

            return Err(Error::Io(std::io::ErrorKind::NotFound.into()));
        }

        let mut key = Self {
            network_key: [0; 16],
        };
        hex::decode_to_slice(raw.network_key, &mut key.network_key)
            .context("failed to decode network key from hex")?;
        Ok(key)
    }
}

/// A cryptographic lemonbeat network
///
/// This holds all necessary keys and can generate inclusion messages.
pub struct Network {
    network_key: NetworkKey,
    public_key: rsa::RsaPublicKey,
}

impl Network {
    /// if `public_key` is [None], an internal default key will be used.
    pub fn new(
        network_key: NetworkKey,
        public_key: Option<rsa::RsaPublicKey>,
    ) -> Result<Self, crate::Error> {
        let public_key = match public_key {
            Some(pk) => pk,
            None => {
                let public_mod = rsa::BigUint::from_bytes_be(&DEFAULT_PUBLIC_KEY[..]);
                rsa::RsaPublicKey::new(public_mod, 65537u64.into())
                    .context("BUG: failed to init public default key")?
            }
        };

        Ok(Self {
            network_key,
            public_key,
        })
    }

    fn inclusion_message_withkey(&self, controller_key: &[u8]) -> Result<Vec<u8>, crate::Error> {
        let cipher = Aes128EcbNopad::new_from_slices(controller_key, &[])
            .context("BUG: failed to parse controller key")?;
        let network_key_encrypted = cipher.encrypt_vec(&self.network_key.network_key);

        let mut message = [0u8; 34];
        message[0..16].copy_from_slice(controller_key);
        message[16..32].copy_from_slice(&network_key_encrypted);

        let mut digest = CRC.digest();
        digest.update(&message[0..32]);
        byteorder::BigEndian::write_u16(&mut message[32..34], digest.finalize());

        let mut rng = rand::rngs::OsRng;
        self.public_key
            .encrypt(&mut rng, Pkcs1v15Encrypt, &message)
            .context("failed to encrypt public key")
    }

    /// Generate a new inclusion message with this network's key
    ///
    /// Internally, a new single-use controller key will be generated.
    pub fn inclusion_message(&self) -> Result<Vec<u8>, crate::Error> {
        let controller_key = generate_key();
        self.inclusion_message_withkey(&controller_key)
    }

    pub fn raw_network_key(&self) -> &[u8] {
        &self.network_key.network_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test_log::test]
    fn basic() {
        let key_binary = hex::decode("00112233445566778899AABBCCDDEEFF").unwrap();
        let network_key = NetworkKey {
            network_key: key_binary.try_into().unwrap(),
        };

        // sample taken from specification - allows to step through inclusion_message_withkey
        // to compare intermediate results with spec as well. Note: spec has a typo and lacks
        // one digit "97127" should be "979127" as in second line below.
        let public_mod = rsa::BigUint::parse_bytes(
            b"\
        544707154624265008293115003105252727119040163562\
        9791278140655528742410706173351390976387790096626\
        006719426326380478518736481632727890811931080151\
        17548459",
            10,
        )
        .unwrap();

        let public_key = rsa::RsaPublicKey::new(public_mod, 65537u64.into()).unwrap();

        let network = Network::new(network_key, Some(public_key)).unwrap();

        let controller_key = hex::decode("0102030405060708090A0B0C0D0E0F00").unwrap();
        let inclusion_message = network.inclusion_message_withkey(&controller_key).unwrap();

        assert_eq!(inclusion_message.len(), 64);
        // cannot assert the inclusion message content as it contains random padding
    }

    #[test_log::test(tokio::test)]
    async fn network_key_persistency() -> Result<(), Error> {
        let file_path = "test_data/network_key_persistency.key";

        let generated_key = NetworkKey::generate();
        let result = generated_key.to_file(file_path).await;
        assert!(result.is_ok());

        if let Ok(loaded_key) = NetworkKey::from_file(file_path) {
            assert_eq!(generated_key.network_key, loaded_key.network_key);
        } else {
            unreachable!();
        }

        fs::remove_file(file_path)?;
        Ok(())
    }
}
