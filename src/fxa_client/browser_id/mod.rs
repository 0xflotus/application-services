use serde_json;

use error::*;

pub mod jwt_utils;
pub mod rsa;

pub trait BrowserIDKeyPair {
  fn private_key(&self) -> &SigningPrivateKey;
  fn public_key(&self) -> &VerifyingPublicKey;
}

pub trait SigningPrivateKey {
  fn get_algo(&self) -> String;
  fn sign(&self, message: &[u8]) -> Result<Vec<u8>>;
}

pub trait VerifyingPublicKey {
  fn to_json(&self) -> serde_json::Value;
}
