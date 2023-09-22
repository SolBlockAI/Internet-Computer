use crate::*;
use ic_crypto_internal_hmac::{Hmac, Sha512};

#[derive(Debug, Clone)]
pub struct DerivationIndex(pub Vec<u8>);

impl DerivationIndex {
    /// Return the BIP32 "next" derivation path
    ///
    /// This is only used very rarely. In the case that a derivation index is a
    /// 4 byte big-endian encoding of an integer less than 2**31-1, the behavior
    /// matches that of standard BIP32.
    ///
    /// For the index 2**31-1, if the exceptional condition occurs, this will
    /// return a BIP32 "hardened" derivation index, which is non-sensical for BIP32.
    /// This is a corner case in the BIP32 spec and it seems that few implementations
    /// handle it correctly.
    pub fn next(&self) -> Self {
        let mut n = self.0.clone();

        n.reverse();

        let mut carry = 1u8;
        for w in &mut n {
            let (v, c) = w.overflowing_add(carry);
            *w = v;
            carry = u8::from(c);
        }

        if carry != 0 {
            n.push(carry);
        }

        n.reverse();

        Self(n)
    }
}

#[derive(Debug, Clone)]
pub struct DerivationPath {
    path: Vec<DerivationIndex>,
}

impl DerivationPath {
    /// The maximum length of a BIP32 derivation path
    ///
    /// The extended public key format uses a byte to represent the derivation
    /// level of a key, thus BIP32 derivations with more than 255 path elements
    /// are not interoperable with other software.
    ///
    /// See https://github.com/bitcoin/bips/blob/master/bip-0032.mediawiki#serialization-format
    /// for details
    pub const MAXIMUM_DERIVATION_PATH_LENGTH: usize = 255;

    /// Create a standard BIP32 derivation path
    pub fn new_bip32(bip32: &[u32]) -> Self {
        let mut path = Vec::with_capacity(bip32.len());
        for n in bip32 {
            path.push(DerivationIndex(n.to_be_bytes().to_vec()));
        }
        Self::new(path)
    }

    /// Create a free-form derivation path
    pub fn new(path: Vec<DerivationIndex>) -> Self {
        Self { path }
    }

    /// Return the length of this path
    pub fn len(&self) -> usize {
        self.path.len()
    }

    /// Return if this path is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn path(&self) -> &[DerivationIndex] {
        &self.path
    }

    /// BIP32 CKD used to implement CKDpub and CKDpriv
    ///
    /// See <https://en.bitcoin.it/wiki/BIP_0032#Child_key_derivation_.28CKD.29_functions>
    ///
    /// Extended to support larger inputs, which is needed for
    /// deriving the canister public key
    ///
    /// This handles both public and private derivation, depending on the value of key_input
    fn bip32_ckd(
        key_input: &[u8],
        curve_type: EccCurveType,
        chain_key: &[u8],
        index: &DerivationIndex,
    ) -> ThresholdEcdsaResult<(Vec<u8>, EccScalar)> {
        // BIP32 is only defined for secp256k1
        if curve_type != EccCurveType::K256 {
            return Err(ThresholdEcdsaError::CurveMismatch);
        }

        let mut hmac = Hmac::<Sha512>::new(chain_key);

        hmac.write(key_input);
        hmac.write(&index.0);

        let hmac_output = hmac.finish();

        let key_offset = EccScalar::from_bytes_wide(curve_type, &hmac_output[..32])?;

        let new_chain_key = hmac_output[32..].to_vec();

        // If iL >= order, try again with the "next" index
        if key_offset.serialize() != hmac_output[..32] {
            Self::bip32_ckd(key_input, curve_type, chain_key, &index.next())
        } else {
            Ok((new_chain_key, key_offset))
        }
    }

    /// BIP32 CKDpub
    ///
    /// See <https://en.bitcoin.it/wiki/BIP_0032#Child_key_derivation_.28CKD.29_functions>
    ///
    /// Extended to support larger inputs, which is needed for
    /// deriving the canister public key
    fn bip32_ckdpub(
        public_key: &EccPoint,
        chain_key: &[u8],
        index: &DerivationIndex,
    ) -> ThresholdEcdsaResult<(EccPoint, Vec<u8>, EccScalar)> {
        let (new_chain_key, key_offset) = Self::bip32_ckd(
            &public_key.serialize(),
            public_key.curve_type(),
            chain_key,
            index,
        )?;

        let new_key = public_key.add_points(&EccPoint::mul_by_g(&key_offset))?;

        // If the new key is infinity, try again with the next index
        if new_key.is_infinity()? {
            return Self::bip32_ckdpub(public_key, chain_key, &index.next());
        }

        Ok((new_key, new_chain_key, key_offset))
    }

    pub fn derive_tweak(
        &self,
        master_public_key: &EccPoint,
    ) -> ThresholdEcdsaResult<(EccScalar, Vec<u8>)> {
        let zeros = [0u8; 32];
        self.derive_tweak_with_chain_code(master_public_key, &zeros)
    }

    pub fn derive_tweak_with_chain_code(
        &self,
        master_public_key: &EccPoint,
        chain_code: &[u8],
    ) -> ThresholdEcdsaResult<(EccScalar, Vec<u8>)> {
        if chain_code.len() != 32 {
            return Err(ThresholdEcdsaError::InvalidArguments(format!(
                "Invalid chain code length {}",
                chain_code.len()
            )));
        }

        if self.len() > Self::MAXIMUM_DERIVATION_PATH_LENGTH {
            return Err(ThresholdEcdsaError::InvalidArguments(format!(
                "Derivation path len {} larger than allowed maximum of {}",
                self.len(),
                Self::MAXIMUM_DERIVATION_PATH_LENGTH
            )));
        }

        let curve_type = master_public_key.curve_type();

        if curve_type == EccCurveType::K256 {
            let mut derived_key = master_public_key.clone();
            let mut derived_chain_key = chain_code.to_vec();
            let mut derived_offset = EccScalar::zero(curve_type);

            for idx in self.path() {
                let (next_derived_key, next_chain_key, next_offset) =
                    Self::bip32_ckdpub(&derived_key, &derived_chain_key, idx)?;

                derived_key = next_derived_key;
                derived_chain_key = next_chain_key;
                derived_offset = derived_offset.add(&next_offset)?;
            }

            Ok((derived_offset, derived_chain_key))
        } else {
            // Key derivation is not currently defined for curves other than secp256k1
            Err(ThresholdEcdsaError::InvalidArguments(format!(
                "Currently key derivation not defined for {}",
                curve_type
            )))
        }
    }
}
