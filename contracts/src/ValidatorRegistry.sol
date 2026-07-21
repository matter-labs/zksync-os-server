// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

/// The on-chain validator registry: the L2 home of consensus committee
/// membership. Consensus nodes do not call this contract — they read its
/// storage slots directly from finalized local state, so the storage layout
/// below is the interface. It is hand-assigned (every field lives at an
/// explicitly chosen slot, written and read through assembly), immune to
/// compiler layout changes, and mirrored constant-for-constant by the node's
/// parser, which golden-tests it byte-for-byte.
///
/// # Storage layout, version 1
///
/// Scalars (small integer slots):
///
///   slot 0  layout version         (uint256, = 1)
///   slot 1  owner                  (address; the governance actor)
///   slot 2  identity count         (uint256)
///   slot 3  schedule entry count   (uint256)
///   slot 4  reserved: DKG ceremony control (inert in v1, no writer exists)
///   slot 5  era anchor             (uint256; consensus height zero)
///   slot 6  epoch length           (uint256; blocks per epoch)
///   slot 7  activation margin      (uint256; minimum epochs of schedule lead, >= 2)
///   slots 8..=15 reserved (zero)
///
/// Slots 5..=7 mirror committee-uniform, fingerprint-pinned node
/// configuration; they exist here only to power the advisory
/// activation-margin guard and are plain storage (not immutables) so the
/// runtime bytecode stays deployment-independent — pinnable byte-for-byte and
/// seedable at genesis without a constructor run.
///
/// Identity table (append-only; identity `i` at base
/// keccak256("zksync-os.registry.v1.identity" ++ uint256(i))):
///
///   base+0  owner address (endpoint self-service rights)
///   base+1  BLS12-381 public key, bytes [0..32)   (48-byte compressed MinPk)
///   base+2  BLS12-381 public key, bytes [32..48) left-aligned
///   base+3  ed25519 public key (32 bytes)
///   base+4  proof of possession, bytes [0..32)    (96-byte BLS signature,
///   base+5  proof of possession, bytes [32..64)    verified node-side; see
///   base+6  proof of possession, bytes [64..96)    the node's registry docs)
///   base+7  ingress socket address, packed (see "Socket packing")
///   base+8  egress address, packed
///   base+9  reserved (zero)
///
/// Key-reservation index (keys are reserved forever — once any identity has
/// used a key, no other identity may, active or not):
///
///   keccak256("zksync-os.registry.v1.bls-key" ++ blsKey[48])   = identity index + 1
///   keccak256("zksync-os.registry.v1.network-key" ++ edKey[32]) = identity index + 1
///
/// Schedule table (append-only; entry `e` at base
/// keccak256("zksync-os.registry.v1.schedule" ++ uint256(e))):
///
///   base+0    activation epoch (uint256; strictly increasing across entries)
///   base+1    member count (uint256)
///   base+2+k  identity index of member k (uint256)
///
/// Reserved for registry v2 (the DKG arc; documented so nothing else claims
/// the range, nothing written in v1):
///
///   keccak256("zksync-os.registry.v1.group-key" ++ uint256(epoch)) — per-epoch
///   threshold group public key (48 bytes over two slots).
///
/// # Socket packing
///
/// One slot per address, big-endian byte layout, left-aligned:
///
///   ingress: [0] = 4 or 6 (IP version), [1..17) = IP (IPv4 in the first four
///            bytes, rest zero), [17..19) = TCP port, remainder zero.
///   egress:  [0] = 4 or 6, [1..17) = IP, remainder zero (no port — it is the
///            address peers see connections *from*).
///
/// # What the contract checks vs. what nodes check
///
/// The contract enforces structure: ownership gates, append-only tables,
/// strictly increasing activation epochs with a minimum lead margin, index
/// validity, key reservation, and ingress uniqueness within an entry. Nodes
/// re-validate everything when deriving a committee — including the BLS proof
/// of possession, which the contract only stores — and refuse to rotate on
/// anything unexpected. The epoch guard here is advisory (it depends on the
/// era anchor and epoch length being configured truthfully at deployment);
/// consensus safety never rests on it.
contract ValidatorRegistry {
    uint256 private constant SLOT_LAYOUT_VERSION = 0;
    uint256 private constant SLOT_OWNER = 1;
    uint256 private constant SLOT_IDENTITY_COUNT = 2;
    uint256 private constant SLOT_SCHEDULE_COUNT = 3;
    uint256 private constant SLOT_ERA_ANCHOR = 5;
    uint256 private constant SLOT_EPOCH_LENGTH = 6;
    uint256 private constant SLOT_ACTIVATION_MARGIN = 7;

    string private constant IDENTITY_PREFIX = "zksync-os.registry.v1.identity";
    string private constant SCHEDULE_PREFIX = "zksync-os.registry.v1.schedule";
    string private constant BLS_KEY_PREFIX = "zksync-os.registry.v1.bls-key";
    string private constant NETWORK_KEY_PREFIX = "zksync-os.registry.v1.network-key";

    /// A committee larger than this is a configuration accident, not a chain.
    uint256 public constant MAX_ENTRY_SIZE = 1024;

    event IdentityRegistered(uint256 indexed index, address indexed owner);
    event EndpointsUpdated(uint256 indexed index);
    event ScheduleEntryAppended(uint256 indexed index, uint256 activationEpoch, uint256 memberCount);
    event OwnershipTransferred(address indexed previousOwner, address indexed newOwner);

    error NotOwner();
    error NotIdentityOwner();
    error ZeroOwner();
    error KeyAlreadyRegistered();
    error UnknownIdentity();
    error BadSocketAddress();
    error EmptyEntry();
    error EntryTooLarge();
    error DuplicateMember();
    error IngressCollision();
    error ActivationNotMonotonic();
    error ActivationTooSoon();

    constructor(address initialOwner, uint256 anchor, uint256 blocksPerEpoch, uint256 marginEpochs) {
        if (initialOwner == address(0)) revert ZeroOwner();
        require(blocksPerEpoch > 0, "epoch length must be positive");
        // Nodes derive epoch T's committee from chain state at the last block
        // of epoch T-2 (a two-epoch lookahead). A smaller margin would let
        // governance append entries the contract accepts but whose lookahead
        // height has already passed — every node would then observe the entry
        // one or more epochs later than its activation epoch claims.
        require(marginEpochs >= 2, "margin must cover the two-epoch lookahead");
        _store(SLOT_LAYOUT_VERSION, bytes32(uint256(1)));
        _store(SLOT_OWNER, bytes32(uint256(uint160(initialOwner))));
        _store(SLOT_ERA_ANCHOR, bytes32(anchor));
        _store(SLOT_EPOCH_LENGTH, bytes32(blocksPerEpoch));
        _store(SLOT_ACTIVATION_MARGIN, bytes32(marginEpochs));
    }

    // ---------------------------------------------------------------- reads

    function layoutVersion() external view returns (uint256) {
        return uint256(_load(SLOT_LAYOUT_VERSION));
    }

    function owner() public view returns (address) {
        return address(uint160(uint256(_load(SLOT_OWNER))));
    }

    function identityCount() public view returns (uint256) {
        return uint256(_load(SLOT_IDENTITY_COUNT));
    }

    function scheduleEntryCount() public view returns (uint256) {
        return uint256(_load(SLOT_SCHEDULE_COUNT));
    }

    /// The registered identity `index`, exactly as laid out in storage.
    function identity(uint256 index)
        external
        view
        returns (
            address identityOwner,
            bytes32 blsKeyHigh,
            bytes32 blsKeyLow,
            bytes32 networkKey,
            bytes32 ingress,
            bytes32 egress
        )
    {
        if (index >= identityCount()) revert UnknownIdentity();
        uint256 base = _identityBase(index);
        identityOwner = address(uint160(uint256(_load(base))));
        blsKeyHigh = _load(base + 1);
        blsKeyLow = _load(base + 2);
        networkKey = _load(base + 3);
        ingress = _load(base + 7);
        egress = _load(base + 8);
    }

    /// The proof of possession stored for identity `index` (nodes verify it;
    /// the contract does not).
    function proofOfPossession(uint256 index) external view returns (bytes32, bytes32, bytes32) {
        if (index >= identityCount()) revert UnknownIdentity();
        uint256 base = _identityBase(index);
        return (_load(base + 4), _load(base + 5), _load(base + 6));
    }

    /// Schedule entry `index`: its activation epoch and member identity indices.
    function scheduleEntry(uint256 index)
        external
        view
        returns (uint256 activationEpoch, uint256[] memory members)
    {
        require(index < scheduleEntryCount(), "unknown schedule entry");
        uint256 base = _scheduleBase(index);
        activationEpoch = uint256(_load(base));
        uint256 count = uint256(_load(base + 1));
        members = new uint256[](count);
        for (uint256 k = 0; k < count; k++) {
            members[k] = uint256(_load(base + 2 + k));
        }
    }

    function eraAnchor() public view returns (uint256) {
        return uint256(_load(SLOT_ERA_ANCHOR));
    }

    function epochLength() public view returns (uint256) {
        return uint256(_load(SLOT_EPOCH_LENGTH));
    }

    function activationMarginEpochs() public view returns (uint256) {
        return uint256(_load(SLOT_ACTIVATION_MARGIN));
    }

    /// The epoch the chain is in right now, per this contract's advisory view
    /// of the epoch geometry.
    function currentEpoch() public view returns (uint256) {
        uint256 anchor = eraAnchor();
        if (block.number < anchor) {
            return 0;
        }
        return (block.number - anchor) / epochLength();
    }

    // --------------------------------------------------------------- writes

    /// Registers a validator identity: its owner, consensus keys, stored (not
    /// verified) proof of possession, and network endpoints. Keys are reserved
    /// forever — a key that ever belonged to any identity can never be
    /// registered again.
    function registerIdentity(
        address identityOwner,
        bytes32 blsKeyHigh,
        bytes32 blsKeyLow,
        bytes32 networkKey,
        bytes32 popA,
        bytes32 popB,
        bytes32 popC,
        bytes32 ingress,
        bytes32 egress
    ) external returns (uint256 index) {
        _requireOwner();
        if (identityOwner == address(0)) revert ZeroOwner();
        _checkSocket(ingress, true);
        _checkSocket(egress, false);

        uint256 blsSlot = _blsKeySlot(blsKeyHigh, blsKeyLow);
        uint256 networkSlot = _networkKeySlot(networkKey);
        if (_load(blsSlot) != bytes32(0) || _load(networkSlot) != bytes32(0)) {
            revert KeyAlreadyRegistered();
        }

        index = identityCount();
        uint256 base = _identityBase(index);
        _store(base, bytes32(uint256(uint160(identityOwner))));
        _store(base + 1, blsKeyHigh);
        _store(base + 2, blsKeyLow);
        _store(base + 3, networkKey);
        _store(base + 4, popA);
        _store(base + 5, popB);
        _store(base + 6, popC);
        _store(base + 7, ingress);
        _store(base + 8, egress);
        _store(SLOT_IDENTITY_COUNT, bytes32(index + 1));
        _store(blsSlot, bytes32(index + 1));
        _store(networkSlot, bytes32(index + 1));
        emit IdentityRegistered(index, identityOwner);
    }

    /// Updates an identity's network endpoints. Self-service: operational data
    /// must not need governance, so the identity's owner can do this alone.
    ///
    /// Deliberately unchecked against other identities' endpoints: an owner who
    /// sets an ingress colliding with a scheduled peer's makes node-side
    /// validation refuse the affected schedule entries (committee changes carry
    /// the previous committee and alarm, naming both identities; the chain is
    /// unaffected, and governance can rotate the offender out with an entry
    /// that excludes them). Acceptable while registration is governance-gated
    /// and owners are vetted; a write-side uniqueness index is required before
    /// registration ever opens up.
    function setEndpoints(uint256 index, bytes32 ingress, bytes32 egress) external {
        if (index >= identityCount()) revert UnknownIdentity();
        uint256 base = _identityBase(index);
        if (msg.sender != address(uint160(uint256(_load(base))))) revert NotIdentityOwner();
        _checkSocket(ingress, true);
        _checkSocket(egress, false);
        _store(base + 7, ingress);
        _store(base + 8, egress);
        emit EndpointsUpdated(index);
    }

    /// Appends a schedule entry: from `activationEpoch` on, the committee is
    /// the listed identities. Entries are append-only and strictly ordered;
    /// the activation must leave the configured lead margin so every node's
    /// lookahead observes the entry before it matters.
    function appendScheduleEntry(uint256 activationEpoch, uint256[] calldata members) external {
        _requireOwner();
        if (members.length == 0) revert EmptyEntry();
        if (members.length > MAX_ENTRY_SIZE) revert EntryTooLarge();

        uint256 entryCount = scheduleEntryCount();
        if (entryCount > 0) {
            uint256 previousActivation = uint256(_load(_scheduleBase(entryCount - 1)));
            if (activationEpoch <= previousActivation) revert ActivationNotMonotonic();
        }
        if (activationEpoch < currentEpoch() + activationMarginEpochs()) revert ActivationTooSoon();

        uint256 identities = identityCount();
        for (uint256 i = 0; i < members.length; i++) {
            if (members[i] >= identities) revert UnknownIdentity();
            bytes32 ingressI = _load(_identityBase(members[i]) + 7);
            for (uint256 j = 0; j < i; j++) {
                if (members[j] == members[i]) revert DuplicateMember();
                if (_load(_identityBase(members[j]) + 7) == ingressI) revert IngressCollision();
            }
        }

        uint256 base = _scheduleBase(entryCount);
        _store(base, bytes32(activationEpoch));
        _store(base + 1, bytes32(members.length));
        for (uint256 k = 0; k < members.length; k++) {
            _store(base + 2 + k, bytes32(members[k]));
        }
        _store(SLOT_SCHEDULE_COUNT, bytes32(entryCount + 1));
        emit ScheduleEntryAppended(entryCount, activationEpoch, members.length);
    }

    function transferOwnership(address newOwner) external {
        _requireOwner();
        if (newOwner == address(0)) revert ZeroOwner();
        address previous = owner();
        _store(SLOT_OWNER, bytes32(uint256(uint160(newOwner))));
        emit OwnershipTransferred(previous, newOwner);
    }

    // -------------------------------------------------------------- helpers

    function _requireOwner() private view {
        if (msg.sender != owner()) revert NotOwner();
    }

    /// Structural check only: the IP version tag must be 4 or 6, an ingress
    /// must carry a port, and bytes reserved as zero must be zero. Nodes do
    /// the semantically meaningful validation.
    function _checkSocket(bytes32 packed, bool requirePort) private pure {
        uint8 kind = uint8(packed[0]);
        if (kind != 4 && kind != 6) revert BadSocketAddress();
        if (kind == 4) {
            // Bytes [5..17) (unused IP space) must be zero.
            if ((uint256(packed) << 40) >> 160 != 0) revert BadSocketAddress();
        }
        uint16 port = (uint16(uint8(packed[17])) << 8) | uint16(uint8(packed[18]));
        if (requirePort && port == 0) revert BadSocketAddress();
        if (!requirePort && port != 0) revert BadSocketAddress();
        // Bytes [19..32) are reserved.
        if (uint256(packed) & ((1 << 104) - 1) != 0) revert BadSocketAddress();
    }

    function _identityBase(uint256 index) private pure returns (uint256) {
        return uint256(keccak256(abi.encodePacked(IDENTITY_PREFIX, index)));
    }

    function _scheduleBase(uint256 index) private pure returns (uint256) {
        return uint256(keccak256(abi.encodePacked(SCHEDULE_PREFIX, index)));
    }

    function _blsKeySlot(bytes32 high, bytes32 low) private pure returns (uint256) {
        return uint256(keccak256(abi.encodePacked(BLS_KEY_PREFIX, high, low)));
    }

    function _networkKeySlot(bytes32 key) private pure returns (uint256) {
        return uint256(keccak256(abi.encodePacked(NETWORK_KEY_PREFIX, key)));
    }

    function _load(uint256 slot) private view returns (bytes32 value) {
        assembly {
            value := sload(slot)
        }
    }

    function _store(uint256 slot, bytes32 value) private {
        assembly {
            sstore(slot, value)
        }
    }
}
