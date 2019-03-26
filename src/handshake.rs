use crate::{
    credential::Credential,
    crypto::{ciphersuite::CipherSuite, dh::DhPublicKey, ecies::EciesCiphertext, sig::Signature},
    group_state::GroupState,
};

// uint8 ProtocolVersion;
pub(crate) type ProtocolVersion = u8;

/// This contains the encrypted `WelcomeInfo` for new group participants
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Welcome {
    // opaque user_init_key_id<0..255>;
    #[serde(rename = "user_init_key_id__bound_u8")]
    user_init_key_id: Vec<u8>,
    pub(crate) cipher_suite: &'static CipherSuite,
    pub(crate) encrypted_welcome_info: EciesCiphertext,
}

/// Contains a node's new public key and the new node's secret, encrypted for everyone in that
/// node's resolution
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DirectPathNodeMessage {
    pub(crate) public_key: DhPublicKey,
    // ECIESCiphertext node_secrets<0..2^16-1>;
    #[serde(rename = "node_secrets__bound_u16")]
    pub(crate) node_secrets: Vec<EciesCiphertext>,
}

/// Contains a direct path of node messages. The length of `node_secrets` for the first
/// `DirectPathNodeMessage` MUST be zero.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct DirectPathMessage {
    // DirectPathNodeMessage nodes<0..2^16-1>;
    #[serde(rename = "node_messages__bound_u16")]
    pub(crate) node_messages: Vec<DirectPathNodeMessage>,
}

/// This is used in lieu of negotiating public keys when a participant is added. This has a bunch
/// of published ephemeral keys that can be used to initiated communication with a previously
/// uncontacted participant.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct UserInitKey {
    // opaque user_init_key_id<0..255>
    /// An identifier for this init key. This MUST be unique among the `UserInitKey` generated by
    /// the client
    #[serde(rename = "user_init_key_id__bound_u8")]
    user_init_key_id: Vec<u8>,

    // ProtocolVersion supported_versions<0..255>;
    /// The protocol versions supported by this client. Each entry is the supported protocol
    /// version of the entry in `init_keys` of the same index. This MUST have the same length as
    /// `init_keys`.
    #[serde(rename = "supported_versions__bound_u8")]
    supported_versions: Vec<ProtocolVersion>,

    // CipherSuite cipher_suites<0..255>
    /// The cipher suites supported by this client. Each cipher suite here corresponds uniquely to
    /// a DH public key in `init_keys`. As such, this MUST have the same length as `init_keys`.
    #[serde(rename = "cipher_suites__bound_u8")]
    pub(crate) cipher_suites: Vec<&'static CipherSuite>,

    // HPKEPublicKey init_keys<1..2^16-1>
    /// The DH public keys owned by this client. Each public key corresponds uniquely to a cipher
    /// suite in `cipher_suites`. As such, this MUST have the same length as `cipher_suites`.
    #[serde(rename = "init_keys__bound_u16")]
    pub(crate) init_keys: Vec<DhPublicKey>,

    /// The identity information of this user
    pub(crate) credential: Credential,

    /// Contains the signature of all the other fields of this struct, under the identity key of
    /// the client.
    pub(crate) signature: Signature,
}

/// This is currently not defined by the spec. See open issue in section 7.1
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupInit;

/// Operation to add a partcipant to a group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupAdd {
    // uint32 index;
    /// Indicates where to add the new participant. This may be a blank node or at index `n` where
    /// `n` is the size of the tree.
    index: u32,

    // UserInitKey init_key;
    /// Contains the public key used to add the new participant
    pub(crate) init_key: UserInitKey,

    // opaque welcome_info_hash<0..255>;
    /// Contains the hash of the `WelcomeInfo` object that preceded this `Add`
    #[serde(rename = "welcome_info_hash__bound_u8")]
    welcome_info_hash: Vec<u8>,
}

/// Operation to add entropy to the group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupUpdate {
    pub(crate) path: DirectPathMessage,
}

/// Operation to remove a partcipant from the group
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct GroupRemove {
    /// The index of the removed participant
    removed: u32,

    /// New entropy for the tree
    pub(crate) path: DirectPathMessage,
}

/// Enum of possible group operations
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename = "GroupOperation__enum_u8")]
pub(crate) enum GroupOperation {
    Init(GroupInit),
    Add(GroupAdd),
    Update(GroupUpdate),
    Remove(GroupRemove),
}

// TODO: Make confirmation a Mac enum for more type safety

/// A `Handshake` message, as defined in section 7 of the MLS spec
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Handshake {
    /// This is equal to the epoch of the current `GroupState`
    pub(crate) prior_epoch: u32,
    /// The operation this `Handshake` is perofrming
    pub(crate) operation: GroupOperation,
    /// Position of the signer in the roster
    pub(crate) signer_index: u32,
    /// Signature over the `Group`'s history:
    /// `Handshake.signature = Sign(identity_key, GroupState.transcript_hash)`
    pub(crate) signature: Signature,
    // opaque confirmation<1..255>;
    /// HMAC over the group state and `Handshake` signature
    /// `confirmation_data = GroupState.transcript_hash || Handshake.signature`
    /// `Handshake.confirmation = HMAC(confirmation_key, confirmation_data)`
    #[serde(rename = "confirmation__bound_u8")]
    pub(crate) confirmation: Vec<u8>,
}

impl Handshake {
    /// Creates a `Handshake` message, given a ciphersuite, group state, and group operation
    fn from_group_op(
        cs: &'static CipherSuite,
        state: &GroupState,
        op: GroupOperation,
    ) -> Handshake {
        // signature = Sign(identity_key, GroupState.transcript_hash)
        let signature = cs.sig_impl.sign(&state.identity_key, &state.transcript_hash);

        // confirmation = HMAC(confirmation_key, confirmation_data)
        // where confirmation_data = GroupState.transcript_hash || Handshake.signature
        let confirmation = {
            let confirmation_key =
                ring::hmac::SigningKey::new(cs.hash_alg, &state.epoch_secrets.confirmation_key);

            let mut ctx = ring::hmac::SigningContext::with_key(&confirmation_key);
            ctx.update(&state.transcript_hash);
            ctx.update(&signature.to_bytes());

            ctx.sign()
        };

        Handshake {
            prior_epoch: state.epoch,
            operation: op,
            signer_index: state.roster_index,
            signature: signature,
            confirmation: confirmation.as_ref().to_vec(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        crypto::{
            ciphersuite::{CipherSuite, P256_SHA256_AES128GCM, X25519_SHA256_AES128GCM},
            sig::SignatureScheme,
        },
        error::Error,
        group_state::WelcomeInfo,
        tls_de::TlsDeserializer,
        upcast::CryptoUpcast,
    };

    use serde::Deserialize;
    use std::io::Read;

    // File: messages.bin
    //
    // struct {
    //   CipherSuite cipher_suite;
    //   SignatureScheme sig_scheme;
    //
    //   opaque user_init_key<0..2^32-1>;
    //   opaque welcome_info<0..2^32-1>;
    //   opaque welcome<0..2^32-1>;
    //   opaque add<0..2^32-1>;
    //   opaque update<0..2^32-1>;
    //   opaque remove<0..2^32-1>;
    // } MessagesCase;
    //
    // struct {
    //   uint32_t epoch;
    //   uint32_t signer_index;
    //   uint32_t removed;
    //   opaque user_id<0..255>;
    //   opaque group_id<0..255>;
    //   opaque uik_id<0..255>;
    //   opaque dh_seed<0..255>;
    //   opaque sig_seed<0..255>;
    //   opaque random<0..255>;
    //
    //   SignatureScheme uik_all_scheme;
    //   UserInitKey user_init_key_all;
    //
    //   MessagesCase case_p256_p256;
    //   MessagesCase case_x25519_ed25519;
    // } MessagesTestVectors;
    //
    // The elements of the struct have the following meanings:
    //
    // * The first several fields contain the values used to construct the example messages.
    // * user_init_key_all contains a UserInitKey that offers all four ciphersuites.  It is validly
    //   signed with an Ed25519 key.
    // * The remaining cases each test message processing for a given ciphersuite:
    //   * case_p256_p256 uses P256 for DH and ECDSA-P256 for signing
    //   * case_x25519_ed25519 uses X25519 for DH and Ed25519 for signing
    // * In each case:
    //   * user_init_key contains a UserInitKey offering only the indicated ciphersuite, validly
    //     signed with the corresponding signature scheme
    //   * welcome_info contains a WelcomeInfo message with syntactically valid but bogus contents
    //   * welcome contains a Welcome message generated by encrypting welcome_info for a
    //     Diffie-Hellman public key derived from the dh_seed value.
    //   * add, update, and remove each contain a Handshake message with a GroupOperation of the
    //     corresponding type.  The signatures on these messages are not valid
    //
    // Your implementation should be able to pass the following tests:
    //
    // * user_init_key_all should parse successfully
    // * The test cases for any supported ciphersuites should parse successfully
    // * All of the above parsed values should survive a marshal / unmarshal round-trip

    #[derive(Debug, Deserialize, Serialize)]
    struct MessagesCase {
        cipher_suite: &'static CipherSuite,
        signature_scheme: &'static SignatureScheme,
        _user_init_key_len: u32,
        user_init_key: UserInitKey,
        _welcome_info_len: u32,
        welcome_info: WelcomeInfo,
        _welcome_len: u32,
        welcome: Welcome,
        _add_len: u32,
        add: Handshake,
        _update_len: u32,
        update: Handshake,
        _remove_len: u32,
        remove: Handshake,
    }

    impl CryptoUpcast for MessagesCase {
        fn upcast_crypto_values(&mut self, ctx: &crate::upcast::CryptoCtx) -> Result<(), Error> {
            let new_ctx =
                ctx.set_cipher_suite(self.cipher_suite).set_signature_scheme(self.signature_scheme);
            self.user_init_key.upcast_crypto_values(&new_ctx);
            self.welcome_info.upcast_crypto_values(&new_ctx);
            self.welcome.upcast_crypto_values(&new_ctx);
            self.add.upcast_crypto_values(&new_ctx);
            self.update.upcast_crypto_values(&new_ctx);
            self.remove.upcast_crypto_values(&new_ctx);
            Ok(())
        }
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct MessagesTestVectors {
        epoch: u32,
        signer_index: u32,
        removed: u32,
        #[serde(rename = "user_id__bound_u8")]
        user_id: Vec<u8>,
        #[serde(rename = "group_id__bound_u8")]
        group_id: Vec<u8>,
        #[serde(rename = "uik_id__bound_u8")]
        uik_id: Vec<u8>,
        #[serde(rename = "dh_seed__bound_u8")]
        dh_seed: Vec<u8>,
        #[serde(rename = "sig_seed__bound_u8")]
        sig_seed: Vec<u8>,
        #[serde(rename = "random__bound_u8")]
        random: Vec<u8>,
        uik_all_scheme: &'static SignatureScheme,
        _user_init_key_all_len: u32,
        user_init_key_all: UserInitKey,

        case_p256_p256: MessagesCase,
        case_x25519_ed25519: MessagesCase,
    }

    impl CryptoUpcast for MessagesTestVectors {
        fn upcast_crypto_values(&mut self, ctx: &crate::upcast::CryptoCtx) -> Result<(), Error> {
            let ctx = ctx.set_signature_scheme(self.uik_all_scheme);
            self.user_init_key_all.upcast_crypto_values(&ctx)?;

            let ctx = ctx.set_cipher_suite(&P256_SHA256_AES128GCM);
            self.case_p256_p256.upcast_crypto_values(&ctx)?;

            let ctx = ctx.set_cipher_suite(&X25519_SHA256_AES128GCM);
            self.case_x25519_ed25519.upcast_crypto_values(&ctx)?;

            Ok(())
        }
    }

    // Tests our code against the official key schedule test vector. All this has to do is make
    // sure that the given test vector parses without error, and that the bytes are the same after
    // being reserialized
    #[test]
    fn official_message_parsing_kat() {
        // Read in the input
        let mut original_bytes = Vec::new();
        let mut f = std::fs::File::open("test_vectors/messages.bin").unwrap();
        f.read_to_end(&mut original_bytes);

        // Deserialize the input
        let mut cursor = original_bytes.as_slice();
        let mut deserializer = TlsDeserializer::from_reader(&mut cursor);
        let test_vec = {
            let mut raw = MessagesTestVectors::deserialize(&mut deserializer).unwrap();
            // We can't do the upcasting here. The documentation lied when it said that
            // UserInitKeys are validly signed. They are [0xd6; 32], which is not a valid Ed25519
            // signature. So skip this step and call it a mission success.
            //raw.upcast_crypto_values(&CryptoCtx::new()).unwrap();
            raw
        };

        // Reserialized the deserialized input and make sure it's the same as the original
        let reserialized_bytes = crate::tls_ser::serialize_to_bytes(&test_vec).unwrap();
        assert_eq!(reserialized_bytes, original_bytes);
    }
}
