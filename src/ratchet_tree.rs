use crate::{
    crypto::{
        ciphersuite::CipherSuite,
        dh::{DhPrivateKey, DhPublicKey},
        ecies, hkdf,
        rng::CryptoRng,
    },
    error::Error,
    handshake::{DirectPathMessage, DirectPathNodeMessage},
    tree_math,
};

// Ratchet trees are serialized in DirectPath messages as optional<PublicKey> tree<1..2^32-1> So we
// encode RatchetTree as a Vec<RatchetTreeNode> with length bound u32, and we encode
// RatchetTreeNode as enum { Blank, Filled { DhPublicKey } }, which is encoded in the same way as
// an Option<DhPublicKey> would be.

/// A node in a `RatchetTree`. Every node must have a DH pubkey. It may also optionally contain the
/// corresponding private key and a secret octet string.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename = "RatchetTreeNode__enum_u8")]
pub(crate) enum RatchetTreeNode {
    Blank,
    Filled {
        public_key: DhPublicKey,
        #[serde(skip)]
        private_key: Option<DhPrivateKey>,
        #[serde(skip)]
        secret: Option<Vec<u8>>,
    },
}

impl RatchetTreeNode {
    /// Returns `true` iff this is the `Filled` variant
    #[rustfmt::skip]
    fn is_filled(&self) -> bool {
        if let RatchetTreeNode::Filled { .. } = self {
            true
        } else {
            false
        }
    }

    /// Updates the node's public key to the given one. This is the only way to convert a `Blank`
    /// node into a `Filled` one.
    pub(crate) fn update_public_key(&mut self, new_public_key: DhPublicKey) {
        match self {
            &mut RatchetTreeNode::Blank => {
                *self = RatchetTreeNode::Filled {
                    public_key: new_public_key,
                    private_key: None,
                    secret: None,
                };
            }
            &mut RatchetTreeNode::Filled {
                ref mut public_key,
                ..
            } => *public_key = new_public_key,
        }
    }

    /// Returns a node's public key. If the node is `Blank`, returns `None`.
    pub(crate) fn get_public_key(&self) -> Option<&DhPublicKey> {
        match self {
            &RatchetTreeNode::Blank => None,
            &RatchetTreeNode::Filled {
                ref public_key,
                ..
            } => Some(public_key),
        }
    }

    /// Updates the node's private key to the given one
    ///
    /// Panics: If the node is `Blank`
    pub(crate) fn update_private_key(&mut self, new_private_key: DhPrivateKey) {
        match self {
            &mut RatchetTreeNode::Blank => panic!("tried to update private key of blank node"),
            &mut RatchetTreeNode::Filled {
                public_key: _,
                ref mut private_key,
                ..
            } => {
                *private_key = Some(new_private_key);
            }
        }
    }

    /// Updates the node's secret to the given one
    ///
    /// Panics: If the node is `Blank`
    pub(crate) fn update_secret(&mut self, new_secret: Vec<u8>) {
        match self {
            &mut RatchetTreeNode::Blank => panic!("tried to update secret of blank node"),
            &mut RatchetTreeNode::Filled {
                public_key: _,
                private_key: _,
                ref mut secret,
            } => {
                *secret = Some(new_secret);
            }
        }
    }

    /// Returns a mutable reference to the contained node secret. If the node is `Filled` and
    /// doesn't have a node secret, one with length `secret_len` is allocated. If the node is
    /// `Blank`, then `None` is returned.
    pub(crate) fn get_mut_node_secret(&mut self, secret_len: usize) -> Option<&mut [u8]> {
        match self {
            &mut RatchetTreeNode::Blank => None,
            &mut RatchetTreeNode::Filled {
                public_key: _,
                private_key: _,
                ref mut secret,
            } => match secret {
                Some(ref mut inner) => Some(inner.as_mut_slice()),
                None => {
                    *secret = Some(vec![0u8; secret_len]);
                    secret.as_mut().map(|v| v.as_mut_slice())
                }
            },
        }
    }

    /// Returns a reference to the contained node secret. If no secret exists, `None` is returned.
    pub(crate) fn get_secret(&self) -> Option<&[u8]> {
        match self {
            &RatchetTreeNode::Blank => None,
            &RatchetTreeNode::Filled {
                public_key: _,
                private_key: _,
                ref secret,
            } => secret.as_ref().map(|v| v.as_slice()),
        }
    }

    /// Returns `Some(&private_key)` if the node contains a private key. Otherwise returns `None`.
    pub(crate) fn get_private_key(&self) -> Option<&DhPrivateKey> {
        match self {
            &RatchetTreeNode::Blank => None,
            &RatchetTreeNode::Filled {
                public_key: _,
                ref private_key,
                ..
            } => private_key.as_ref(),
        }
    }
}

/// A left-balanced binary tree of `RatchetTreeNode`s
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct RatchetTree {
    #[serde(rename = "nodes__bound_u32")]
    pub(crate) nodes: Vec<RatchetTreeNode>,
}

impl RatchetTree {
    /// Returns an new empty `RatchetTree`
    pub fn new() -> RatchetTree {
        RatchetTree {
            nodes: Vec::new(),
        }
    }

    /// Returns the number of nodes in the tree
    pub fn size(&self) -> usize {
        self.nodes.len()
    }

    /// Returns the node at the given index
    pub fn get(&self, idx: usize) -> Option<&RatchetTreeNode> {
        self.nodes.get(idx)
    }

    /// Returns the root node. Returns `None` iff the tree is empty.
    pub fn get_root_node(&self) -> Option<&RatchetTreeNode> {
        if self.size() == 0 {
            None
        } else {
            let root_idx = tree_math::root_idx(self.size());
            self.get(root_idx)
        }
    }

    /// Returns a mutable reference to the node at the given index
    pub fn get_mut(&mut self, idx: usize) -> Option<&mut RatchetTreeNode> {
        self.nodes.get_mut(idx)
    }

    // It turns out that appending to the tree in this way preserves the left-balanced property
    // while keeping everything in place. Instead of a proof, stare this diagram where I add a new
    // leaf node to a tree of 3 leaves, and then add another leaf to that. The stars represent
    // non-leaf nodes.
    //         *                   *                        *
    //       /   \               /   \                _____/ \
    //      /     C   Add(D)    /     \    Add(E)    /        |
    //     *          =====>   *       *   =====>   *         |
    //    / \                 / \     / \         /   \       |
    //   A   B               A   B   C   D       /     \      |
    //   0 1 2 3  4          0 1 2 3 4 5 6      *       *     |
    //                                         / \     / \    |
    //                                        A   B   C   D   E
    //                                        0 1 2 3 4 5 6 7 8
    pub fn add_leaf_node(&mut self, node: RatchetTreeNode) {
        if self.nodes.is_empty() {
            self.nodes.push(node);
            return;
        } else {
            self.nodes.push(RatchetTreeNode::Blank);
            self.nodes.push(node);
        }
    }

    /// Blanks out the direct path of the given node, as well as the root node
    pub(crate) fn propogate_blank(&mut self, start_idx: usize) {
        let num_leaves = tree_math::num_leaves_in_tree(self.size());
        let direct_path = tree_math::node_direct_path(start_idx, num_leaves);

        // Blank the direct path
        for i in direct_path {
            // No need to check index here. By construction, there's no way this is out of bounds
            self.nodes[i] = RatchetTreeNode::Blank;
        }

        // Blank the root
        let root_idx = tree_math::root_idx(num_leaves);
        self.nodes[root_idx] = RatchetTreeNode::Blank;
    }

    // This always produces a valid tree. To see this, note that truncating to a leaf node when
    // there are >1 non-blank leaf nodes gives you a vector of odd length. All vectors of odd
    // length have a unique interpretation as a binary left-balanced tree. And if there are no
    // non-blank leaf nodes, you get an empty tree.
    /// Truncates the tree down to the first non-blank leaf node
    pub(crate) fn truncate_to_last_nonblank(&mut self) {
        let num_leaves = tree_math::num_leaves_in_tree(self.size());

        let mut last_nonblank_leaf = None;
        for leaf_idx in tree_math::tree_leaves(num_leaves).rev() {
            if self.nodes[leaf_idx].is_filled() {
                last_nonblank_leaf = Some(leaf_idx);
            }
        }

        match last_nonblank_leaf {
            // If there are no nonempty entries in the roster, clear it
            None => self.nodes.clear(),
            Some(i) => {
                // This can't fail, because i is an index
                let num_elements_to_retain = i + 1;
                self.nodes.truncate(num_elements_to_retain)
            }
        }
    }

    /// Returns the indices of the resolution of a given node: this an ordered sequence of minimal
    /// set of non-blank nodes that collectively cover (A "covers" B iff A is an ancestor of B) all
    /// non-blank descendants of the given node. The ordering is ascending by node index.
    pub(crate) fn resolution(&self, idx: usize) -> Vec<usize> {
        // Helper function that accumulates the resolution recursively
        fn helper(tree: &RatchetTree, i: usize, acc: &mut Vec<usize>) {
            if let RatchetTreeNode::Blank = tree.nodes[i] {
                if tree_math::node_level(i) == 0 {
                    // The resolution of a blank leaf node is the empty list
                    return;
                } else {
                    // The resolution of a blank intermediate node is the result of concatinating
                    // the resolution of its left child with the resolution of its right child, in
                    // that order
                    let num_leaves = tree_math::num_leaves_in_tree(tree.nodes.len());
                    helper(tree, tree_math::node_left_child(i), acc);
                    helper(tree, tree_math::node_right_child(i, num_leaves), acc);
                }
            } else {
                // The resolution of a non-blank node is a one element list containing the node
                // itself
                acc.push(i);
            }
        }

        let mut ret = Vec::new();
        helper(self, idx, &mut ret);
        ret
    }

    /// Given a node with a known secret, constructs a `DirectPathMessage` containing encrypted
    /// copies of the appropriately ratcheted secret for the rest of the ratchet tree. See section
    /// 5.2 in the spec for details.
    ///
    /// Requires: `my_leaf_idx` to be a leaf node. Otherwise, any child of ours would be unable to
    /// decrypt this message.
    pub(crate) fn encrypt_direct_path_secrets(
        &self,
        cs: &'static CipherSuite,
        my_leaf_idx: usize,
        csprng: &mut dyn CryptoRng,
    ) -> Result<DirectPathMessage, Error> {
        // Check if it's a leaf node
        if my_leaf_idx % 2 != 0 {
            return Err(Error::TreeError("Cannot encrypt direct paths of non-leaf nodes"));
        }

        let num_leaves = tree_math::num_leaves_in_tree(self.size());
        let direct_path = tree_math::node_direct_path(my_leaf_idx as usize, num_leaves);

        let mut node_messages = Vec::new();

        // The first node message should be just my public key
        let my_public_key = self
            .get(my_leaf_idx)
            .ok_or(Error::TreeError("My tree index isn't in the tree"))?
            .get_public_key()
            .ok_or(Error::TreeError("My tree index is blank"))?;
        node_messages.push(DirectPathNodeMessage {
            public_key: my_public_key.clone(),
            node_secrets: Vec::with_capacity(0),
        });

        // Go up the direct path of my_leaf_idx
        for path_node_idx in direct_path {
            // For each node in our direct path, we need to encrypt the parent's new node_secret
            // for the sibling, and also include the parent's public key
            let parent_path_node_idx = tree_math::node_parent(path_node_idx, num_leaves);
            let parent_path_node = self.get(parent_path_node_idx).unwrap();
            let parent_public_key = parent_path_node
                .get_public_key()
                .ok_or(Error::TreeError("Non-blank node has a blank parent"))?;
            let parent_secret = parent_path_node
                .get_secret()
                .ok_or(Error::TreeError("Node doesn't know its parent's secret"))?;

            // Encrypt the secret of the current node for everyone in the resolution of the
            // copath node. We can unwrap() here because self.resolution only returns indices that
            // are actually in the tree.
            let mut node_secrets = Vec::new();
            let copath_node_idx = tree_math::node_sibling(path_node_idx, num_leaves);
            for res_node in self.resolution(copath_node_idx).iter().map(|&i| &self.nodes[i]) {
                // We can unwrap() here because self.resolution only returns indices of nodes
                // that are non-blank, by definition of "resolution"
                let others_public_key = res_node.get_public_key().unwrap();
                let ciphertext =
                    ecies::ecies_encrypt(cs, others_public_key, parent_secret.to_vec(), csprng)?;
                node_secrets.push(ciphertext);
            }

            // Push the collection to the message list
            node_messages.push(DirectPathNodeMessage {
                public_key: parent_public_key.clone(),
                node_secrets: node_secrets,
            });
        }

        Ok(DirectPathMessage {
            node_messages,
        })
    }

    /// Finds the (unique) ciphertext in the given direct path message that is meant for this
    /// participant and decrypts it. `sender_tree_idx` is the the index of the creator of `msg`,
    /// and `my_tree_idx` is the index of the decryptor.
    ///
    /// Requires: `sender_tree_idx` cannot be an ancestor of `my_tree_idx`, nor vice-versa. We
    /// cannot decrypt messages that violate this.
    ///
    /// Returns: `Ok((pt, idx))` where `pt` is the `Result` of decrypting the found ciphertext and
    /// `idx` is the common ancestor of `sender_tree_idx` and `my_tree_idx`. If no decryptable
    /// ciphertext exists, returns an `Error::TreeError`. If decryption fails, returns an
    /// `Error::EncryptionError`.
    pub(crate) fn decrypt_direct_path_message(
        &self,
        cs: &'static CipherSuite,
        direct_path_msg: &DirectPathMessage,
        sender_tree_idx: usize,
        my_tree_idx: usize,
    ) -> Result<(Vec<u8>, usize), Error> {
        let num_leaves = tree_math::num_leaves_in_tree(self.size());
        let direct_path = tree_math::node_direct_path(sender_tree_idx, num_leaves);

        if sender_tree_idx >= self.size() || my_tree_idx >= self.size() {
            return Err(Error::TreeError("Input index out of range"));
        }

        if tree_math::is_ancestor(sender_tree_idx, my_tree_idx, num_leaves)
            || tree_math::is_ancestor(my_tree_idx, sender_tree_idx, num_leaves)
        {
            return Err(Error::TreeError("Cannot decrypt messages from ancestors or descendants"));
        }

        // This is the intermediate node in the direct path whose secret was encrypted for us.
        let common_ancestor_idx =
            tree_math::common_ancestor(sender_tree_idx, my_tree_idx, num_leaves);

        // This holds the secret of the intermediate node, encrypted for all the nodes in the
        // resolution of the copath node.
        let node_msg = {
            // To get this value, we have to figure out the correct index into node_message
            let (pos_in_msg_vec, _) =
                tree_math::node_extended_direct_path(sender_tree_idx, num_leaves)
                    .enumerate()
                    .find(|&(_, dp_idx)| dp_idx == common_ancestor_idx)
                    .expect("common ancestor somehow did not appear in direct path");
            direct_path_msg
                .node_messages
                .get(pos_in_msg_vec)
                .ok_or(Error::TreeError("Malformed DirectPathMessage"))?
        };

        // This is the unique acnestor of the receiver that is in the copath of the sender. This is
        // the one whose resolution is used.
        let copath_ancestor_idx = {
            let left = tree_math::node_left_child(common_ancestor_idx);
            let right = tree_math::node_right_child(common_ancestor_idx, num_leaves);
            if tree_math::is_ancestor(left, my_tree_idx, num_leaves) {
                left
            } else {
                right
            }
        };

        // We're looking for an ancestor in the resolution of this copath node. There is
        // only one such node. Furthermore, we should already know the private key of the
        // node that we find. So our strategy is to look for a node with a private key that
        // we know, then make sure that it is our ancestor.
        let resolution = self.resolution(copath_ancestor_idx);

        // Comb the resolution for a node whose private key we know
        for (pos_in_res, res_node_idx) in resolution.into_iter().enumerate() {
            let res_node = self.get(res_node_idx).expect("resolution out of bounds");
            if res_node.get_private_key().is_some()
                && tree_math::is_ancestor(res_node_idx, my_tree_idx, num_leaves)
            {
                // We found the ancestor in the resolution. Now get the decryption key and
                // corresopnding ciphertext
                let decryption_key = res_node.get_private_key().unwrap();
                let ciphertext_for_me = node_msg
                    .node_secrets
                    .get(pos_in_res)
                    .ok_or(Error::TreeError("Malformed DirectPathMessage"))?;

                // Finally, decrypt the thing and return the plaintext and common ancestor
                let pt = ecies::ecies_decrypt(cs, decryption_key, ciphertext_for_me.clone())?;
                return Ok((pt, common_ancestor_idx));
            }
        }

        return Err(Error::TreeError("Cannot find node in resolution with known private key"));
    }

    /// Updates the secret of the node at the given index and derives the path secrets, node
    /// secrets, private keys, and public keys of all its ancestors. If this process fails, this
    /// method will _not_ roll back the operation, so the caller should expect this object to be in
    /// an invalid state.
    pub(crate) fn propogate_new_path_secret(
        &mut self,
        cs: &'static CipherSuite,
        mut path_secret: Vec<u8>,
        start_idx: usize,
    ) -> Result<(), Error> {
        let num_leaves = tree_math::num_leaves_in_tree(self.size());
        let root_node_idx = tree_math::root_idx(num_leaves);

        let node_secret_len = cs.hash_alg.output_len;
        let mut current_node_idx = start_idx;

        // Go up the tree, setting the node secrets and keypairs
        loop {
            let current_node =
                self.get_mut(current_node_idx).expect("reached invalid node in secret propogation");

            let prk = hkdf::prk_from_bytes(cs.hash_alg, &path_secret);
            // node_secret[n] = HKDF-Expand-Label(path_secret[n], "node", "", Hash.Length)
            let mut node_secret = vec![0u8; node_secret_len];
            hkdf::hkdf_expand_label(&prk, b"node", b"", node_secret.as_mut_slice());
            // path_secret[n] = HKDF-Expand-Label(path_secret[n-1], "path", "", Hash.Length)
            hkdf::hkdf_expand_label(&prk, b"path", b"", path_secret.as_mut_slice());

            // Derive the private and public keys and assign them to the node
            let (node_public_key, node_private_key) = cs.derive_key_pair(&node_secret)?;
            current_node.update_public_key(node_public_key);
            current_node.update_private_key(node_private_key);
            current_node.update_secret(node_secret);

            if current_node_idx == root_node_idx {
                // If we just updated the root, we're done
                break;
            } else {
                // Otherwise, take one step up the tree
                current_node_idx = tree_math::node_parent(current_node_idx, num_leaves);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::{
        crypto::{
            ciphersuite::X25519_SHA256_AES128GCM,
            dh::{DhPublicKey, DhPublicKeyRaw, DiffieHellman},
        },
        tls_de::TlsDeserializer,
    };

    use quickcheck::TestResult;
    use quickcheck_macros::quickcheck;
    use rand::Rng;
    use rand_core::SeedableRng;
    use serde::Deserialize;

    // The following test vector is from
    // https://github.com/mlswg/mls-implementations/tree/master/test_vectors
    //
    // File: resolution.bin
    //
    // uint8_t Resolution<0..255>;
    // Resolution ResolutionCase<0..2^16-1>;
    //
    // struct {
    //   uint32_t n_leaves;
    //   ResolutionCase cases<0..2^32-1>;
    // } ResolutionTestVectors;
    //
    // These vectors represent the output of the resolution algorithm on all configurations of a
    // tree with n_leaves leaves.
    //
    // * The cases vector should have 2^(2*n_leaves - 1) entries
    //   * The entry at index t represents the set of resolutions for the tree with a blank /
    //     filled pattern matching the bit pattern of the integer t.
    //   * If (t >> n) == 1, then node n in the tree is filled; otherwise it is blank.
    // * Each ResolutionCase vector contains the resolutions of every node in the tree, in order
    // * Thus cases[t][i] contains the resolution of node i in tree t
    //
    // Your implementation should be able to reproduce these values.
    // Parses the bits of a u32 from right to left, interpreting a 0 as a Blank node and 1 as a
    // Filled node (unimportant what the pubkey is)

    #[derive(Debug, Deserialize)]
    #[serde(rename = "Resolution__bound_u8")]
    struct Resolution(Vec<u8>);

    #[derive(Debug, Deserialize)]
    #[serde(rename = "ResolutionCase__bound_u16")]
    struct ResolutionCase(Vec<Resolution>);

    #[derive(Debug, Deserialize)]
    struct ResolutionTestVectors {
        num_leaves: u32,
        #[serde(rename = "cases__bound_u32")]
        cases: Vec<ResolutionCase>,
    }

    // Test that decrypt_direct_path_message is the inverse of encrypt_direct_path_secrets
    //#[quickcheck]
    //fn direct_path_message_correctness(num_leaves: u8, rng_seed: u64) -> TestResult {
    #[test]
    fn direct_path_message_correctness() {
        let num_leaves = 7;
        let rng_seed = 36;
        // Turns out this test is super slow
        if num_leaves > 50 || num_leaves < 2 {
            return;
        }

        let mut rng = rand::rngs::StdRng::seed_from_u64(rng_seed);
        let num_leaves = num_leaves as usize;
        let num_nodes = tree_math::num_nodes_in_tree(num_leaves);
        let root_idx = tree_math::root_idx(num_leaves);

        // Fill a tree with Blanks
        let mut tree = RatchetTree::new();
        for _ in 0..num_leaves {
            tree.add_leaf_node(RatchetTreeNode::Blank);
        }

        // Fill the tree with deterministic path secrets
        let cs: &'static CipherSuite = &X25519_SHA256_AES128GCM;
        for i in 0..num_leaves {
            let leaf_idx = 2 * i;
            let initial_path_secret = vec![i as u8; 32];
            tree.propogate_new_path_secret(cs, initial_path_secret, leaf_idx);
        }

        // Come up with sender and receiver indices. The sender must be a leaf node, because
        // encryption function requires it. The receiver must be different from the sender, because
        // the decryption function requires it. Also the receiver cannot be an ancestor of the
        // sender, because then it doesn't lie in the copath (and also it would have no need to
        // decrypt the message, since it knows its own secret)
        let sender_tree_idx = 2 * rng.gen_range(0, num_leaves);
        let receiver_tree_idx = loop {
            let idx = rng.gen_range(0, num_nodes);
            if idx != sender_tree_idx && !tree_math::is_ancestor(idx, sender_tree_idx, num_leaves) {
                break idx;
            }
        };

        // Encrypt the sender's direct path secrets
        let direct_path_msg = tree
            .encrypt_direct_path_secrets(cs, sender_tree_idx, &mut rng)
            .expect("failed to encrypt direct path secrets");
        // Decrypt the path secret closest to the receiver
        let (derived_path_secret, common_ancestor_idx) = tree
            .decrypt_direct_path_message(cs, &direct_path_msg, sender_tree_idx, receiver_tree_idx)
            .expect("failed to decrypt direct path secret");

        // Make sure it really is the common ancestor
        assert_eq!(
            common_ancestor_idx,
            tree_math::common_ancestor(sender_tree_idx, receiver_tree_idx, num_leaves)
        );

        // The path secret is precisely the secret of the common ancestor of the sender and
        // receiver.
        let expected_path_secret = tree.get(common_ancestor_idx).unwrap().get_secret().unwrap();
        assert_eq!(derived_path_secret, expected_path_secret);
    }

    // Tests against the official tree math test vector. See above comment for explanation.
    #[test]
    fn official_resolution_kat() {
        // Helper function
        fn u8_resolution(tree: &RatchetTree, idx: usize) -> Vec<u8> {
            tree.resolution(idx)
                .into_iter()
                .map(|i| {
                    // These had better be small indices
                    if i > core::u8::MAX as usize {
                        panic!("resolution node indices are too big to fit into a u8");
                    } else {
                        i as u8
                    }
                })
                .collect()
        }

        // Helper function
        fn make_tree_from_int(t: usize, num_nodes: usize) -> RatchetTree {
            let mut nodes: Vec<RatchetTreeNode> = Vec::new();
            let mut bit_mask = 0x01;

            for _ in 0..num_nodes {
                if t & bit_mask == 0 {
                    nodes.push(RatchetTreeNode::Blank);
                } else {
                    // TODO: Make a better way to put dummy values in the tree than invalid DH pubkeys
                    nodes.push(RatchetTreeNode::Filled {
                        public_key: DhPublicKey::Raw(DhPublicKeyRaw(Vec::new())),
                        private_key: None,
                        secret: None,
                    });
                }
                bit_mask <<= 1;
            }

            RatchetTree {
                nodes,
            }
        }

        let mut f = std::fs::File::open("test_vectors/resolution.bin").unwrap();
        let mut deserializer = TlsDeserializer::from_reader(&mut f);
        let test_vec = ResolutionTestVectors::deserialize(&mut deserializer).unwrap();
        let num_nodes = tree_math::num_nodes_in_tree(test_vec.num_leaves as usize);

        // encoded_tree is the index into the case; this can be decoded into a RatchetTree by
        // parsing the u32 bit by bit
        for (encoded_tree, case) in test_vec.cases.into_iter().enumerate() {
            let tree = make_tree_from_int(encoded_tree, num_nodes);

            // We compute the resolution of every node in the tree
            for (idx, expected_resolution) in case.0.into_iter().enumerate() {
                let derived_resolution = u8_resolution(&tree, idx);
                assert_eq!(derived_resolution, expected_resolution.0);
            }
        }
    }
}
