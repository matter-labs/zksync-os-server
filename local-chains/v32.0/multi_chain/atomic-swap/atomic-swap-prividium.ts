/**
 * Two-party atomic swap driven entirely through the Prividium interop API.
 *
 * The sibling `atomic-swap-two-party.ts` implements the whole protocol client-side: it builds legs,
 * exchanges bundles and proofs over an in-process channel, waits for roots and executes. This script
 * does none of that. It only performs the four steps a *user* performs, and lets each Prividium
 * instance's worker do everything in between:
 *
 *   1. propose   POST /interop/proposals                    (Alice, instance A)
 *   2. approve   POST /interop/proposals/:id/approve        (each user, on their own instance)
 *   3. commit    GET  /interop/proposals/:id/commit-params  -> sign in the wallet, send
 *   4. watch     GET  /interop/proposals/:id                until `completed`
 *
 * So a green run here also proves the two instances relay terms, share bundles, derive the same
 * flowId, exchange proofs and execute — none of which this script touches.
 *
 * Expected setup (zksync-prividium/docker-compose-dual.yaml):
 *   - chain A on :3050 (6565) behind permissions-api :8000; chain B on :3051 (6566) behind :8300
 *     (the sequencers are never contacted directly — all chain access goes through `<origin>/rpc`)
 *   - INTEROP_ENABLED on both, each with an active signing key and the other allowlisted
 *   - the chain pair registered on L1 in BOTH directions (a leg X->Y needs chain X to know Y)
 *   - `pnpm db:seed:wallet-auth` applied to both databases — the wallets below are seeded users.
 *     Each leg's `senderAddress` must be a wallet linked on that leg's SOURCE instance: ownership is
 *     derived from it (source chain + linked wallet), so an unlinked wallet makes the proposal 404
 *     for its own participant.
 *
 * Usage:
 *   TOKEN_A=0x... TOKEN_B=0x... npx ts-node atomic-swap-prividium.ts
 */

import { ethers } from 'ethers';
import { ERC20Abi, L2_NATIVE_TOKEN_VAULT_ADDRESS, NativeTokenVaultAbi, computeAssetId } from './lib';

// --- Config ---------------------------------------------------------------------------------------

interface Side {
    label: string;
    apiUrl: string;
    /** A domain the instance accepts for SIWE (see SIWE_VALID_DOMAINS). */
    siweDomain: string;
    /**
     * The instance's RPC proxy, NOT the sequencer: a Prividium user has no route to the sequencer.
     * Mounted at the origin root rather than under the `/api` prefix the other routes share.
     */
    rpcUrl: string;
    chainId: bigint;
    /** Seeded crypto-native users from db:seed:wallet-auth — see each side's note below. */
    privateKey: string;
    token: string;
}

function requireEnv(name: string): string {
    const value = process.env[name];
    if (!value) throw new Error(`${name} is required — an ERC20 that sender holds on that chain`);
    return value;
}

const A: Side = {
    label: 'Alice',
    apiUrl: process.env.A_API_URL ?? 'http://localhost:8000/api',
    siweDomain: process.env.A_SIWE_DOMAIN ?? 'localhost:3000',
    rpcUrl: process.env.A_RPC_URL ?? 'http://localhost:8000/rpc',
    chainId: BigInt(process.env.A_CHAIN_ID ?? 6565),
    // Seed's "Test User" (0x3C44…), deliberately NOT the admin wallet: docker-compose-dual.yaml uses
    // anvil #0 as both INTEROP_OPERATOR_PRIVATE_KEY and FAUCET_OPERATOR_PRIVATE_KEY, so using it here
    // would make Alice indistinguishable from the instance's own operator — her account would appear
    // to send executeAtomicBundle, and funding her would silently fund the worker.
    privateKey: process.env.ALICE_PRIVATE_KEY ?? '0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a',
    token: requireEnv('TOKEN_A')
};

const B: Side = {
    label: 'Bob',
    apiUrl: process.env.B_API_URL ?? 'http://localhost:8300/api',
    siweDomain: process.env.B_SIWE_DOMAIN ?? 'localhost:3300',
    rpcUrl: process.env.B_RPC_URL ?? 'http://localhost:8300/rpc',
    chainId: BigInt(process.env.B_CHAIN_ID ?? 6566),
    privateKey: process.env.BOB_PRIVATE_KEY ?? '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d',
    token: requireEnv('TOKEN_B')
};

const AMOUNT = ethers.parseUnits(process.env.AMOUNT ?? '100', 18);
const POLL_INTERVAL_MS = 3_000;
const POLL_TIMEOUT_MS = 15 * 60_000;

const NTV_EXTRA_ABI = [
    'function ensureTokenIsRegistered(address _nativeToken) returns (bytes32)',
    'function assetId(address token) view returns (bytes32)',
    'function tokenAddress(bytes32 _assetId) view returns (address)'
];

// --- Shapes mirrored from the API's routes/schemas/interop.ts --------------------------------------

interface CommitTx {
    legIndex: number;
    to: string;
    data: string;
    value: string;
    chainId: string;
}

interface ProposalLeg {
    legIndex: number;
    sourceChainId: string;
    destinationChainId: string;
    assetId: string;
    amount: string;
    recipient: string;
    senderAddress: string;
    approved: boolean;
}

interface Proposal {
    proposalId: string;
    status: string;
    flowId: string | null;
    deadline: string;
    settlementLayerChainId: string;
    legs: ProposalLeg[];
}

// --- One user, on one instance, with one chain -----------------------------------------------------

class Participant {
    // Both stay unconnected until `login()`: the proxy needs the session token on every request, so
    // there is no chain access to be had before signing in.
    private connection: { provider: ethers.JsonRpcProvider; wallet: ethers.Wallet } | null = null;
    private readonly signer: ethers.Wallet;
    private token: string | null = null;

    constructor(readonly side: Side) {
        this.signer = new ethers.Wallet(side.privateKey);
    }

    private get provider(): ethers.JsonRpcProvider {
        if (!this.connection) throw new Error(`[${this.side.label}] not authenticated`);
        return this.connection.provider;
    }

    private get wallet(): ethers.Wallet {
        if (!this.connection) throw new Error(`[${this.side.label}] not authenticated`);
        return this.connection.wallet;
    }

    get address(): string {
        return this.signer.address;
    }

    /** SIWE login: fetch a challenge, sign it, exchange it for a session token. */
    async login(): Promise<void> {
        const challenge = await this.api<{ msg: string; nonceToken: string }>(
            'POST',
            '/siwe-messages',
            { address: this.address, domain: this.side.siweDomain },
            { anonymous: true }
        );
        const signature = await this.signer.signMessage(challenge.msg);
        const auth = await this.api<{ token?: string }>(
            'POST',
            '/auth/login/crypto-native',
            { message: challenge.msg, signature, nonceToken: challenge.nonceToken },
            { anonymous: true }
        );
        if (!auth.token) throw new Error(`[${this.side.label}] login returned no token — MFA required for this user?`);
        this.token = auth.token;

        // Every chain read and write from here on goes through the instance's RPC proxy, carrying the
        // session token — the same path a real user's wallet would take. staticNetwork avoids an
        // eth_chainId round trip per call.
        const request = new ethers.FetchRequest(this.side.rpcUrl);
        request.setHeader('authorization', `Bearer ${this.token}`);
        const provider = new ethers.JsonRpcProvider(request, Number(this.side.chainId), {
            staticNetwork: true
        });
        this.connection = { provider, wallet: this.signer.connect(provider) };

        const proxied = await provider.getNetwork();
        if (proxied.chainId !== this.side.chainId) {
            throw new Error(
                `[${this.side.label}] proxy at ${this.side.rpcUrl} serves chain ${proxied.chainId}, expected ${this.side.chainId}`
            );
        }
        console.log(`[${this.side.label}] signed in as ${this.address} (rpc ${this.side.rpcUrl})`);
    }

    async api<T>(method: string, path: string, body?: unknown, opts: { anonymous?: boolean } = {}): Promise<T> {
        const headers: Record<string, string> = { 'content-type': 'application/json' };
        if (!opts.anonymous) {
            if (!this.token) throw new Error(`[${this.side.label}] not authenticated`);
            headers.authorization = `Bearer ${this.token}`;
        }
        const res = await fetch(`${this.side.apiUrl}${path}`, {
            method,
            headers,
            body: body === undefined ? undefined : JSON.stringify(body)
        });
        const text = await res.text();
        if (!res.ok) {
            throw new Error(`[${this.side.label}] ${method} ${path} -> ${res.status}: ${text.slice(0, 400)}`);
        }
        return (text ? JSON.parse(text) : undefined) as T;
    }

    /**
     * Register the token with the NTV and approve the vault — prerequisites for the leg to be *built*
     * at all, since the instance builds it by simulating the real send, which runs the burn path.
     */
    async prepareToken(): Promise<string> {
        const ntv = new ethers.Contract(
            L2_NATIVE_TOKEN_VAULT_ADDRESS,
            [...NativeTokenVaultAbi, ...NTV_EXTRA_ABI],
            this.wallet
        );
        await (await ntv.ensureTokenIsRegistered(this.side.token)).wait();
        // Approve well above AMOUNT on purpose. The instance builds a leg by simulating the real send,
        // and it re-simulates after the leg has already committed — by which point an exact-AMOUNT
        // approval is spent, so the simulation reverts and the flow wedges in 'committing' forever.
        // The revert surfaces as "is the destination chain registered?", which points nowhere near it.
        await (
            await new ethers.Contract(this.side.token, ERC20Abi, this.wallet).approve(
                L2_NATIVE_TOKEN_VAULT_ADDRESS,
                AMOUNT * 10n
            )
        ).wait();

        // Read the id the vault assigned rather than deriving it: this is the value that ends up in the
        // bundle, and the counterparty checks it against the agreed terms.
        const assetId: string = await ntv.assetId(this.side.token);
        const derived = computeAssetId(this.side.chainId, L2_NATIVE_TOKEN_VAULT_ADDRESS, this.side.token);
        if (assetId.toLowerCase() !== derived.toLowerCase()) {
            // Expected when the token is bridged rather than native here — the id encodes its origin.
            console.warn(`[${this.side.label}] vault assetId ${assetId} != locally derived ${derived}`);
        }
        console.log(`[${this.side.label}] ${this.side.token} registered + approved, assetId ${assetId}`);
        return assetId;
    }

    /**
     * The proposal as this instance sees it, or null while it 404s. A 404 means either "not relayed
     * here yet" or "none of its legs are yours" — the API deliberately does not distinguish, so that
     * others' proposals are never disclosed. Any other status is a real failure and propagates.
     */
    async find(proposalId: string): Promise<Proposal | null> {
        try {
            return await this.api<Proposal>('GET', `/interop/proposals/${proposalId}`);
        } catch (err) {
            if (err instanceof Error && err.message.includes('-> 404')) return null;
            throw err;
        }
    }

    /** Sign and send the commit transaction(s) the instance prepared — verbatim. */
    async commit(commits: CommitTx[]): Promise<void> {
        for (const commit of commits) {
            if (BigInt(commit.chainId) !== this.side.chainId) {
                throw new Error(`[${this.side.label}] commit targets chain ${commit.chainId}, not ${this.side.chainId}`);
            }
            // Verbatim matters: rebuilding any part (the salt above all) changes the bundle hash, which
            // is then absent from legBundleHashes, and the commit reverts.
            const tx = await this.wallet.sendTransaction({
                to: commit.to,
                data: commit.data,
                value: BigInt(commit.value)
            });
            const receipt = await tx.wait();
            if (receipt!.status !== 1) throw new Error(`[${this.side.label}] commit reverted (${tx.hash})`);
            console.log(`[${this.side.label}] committed leg ${commit.legIndex} (${tx.hash})`);
        }
    }

    async wrappedBalance(assetId: string): Promise<{ token: string; balance: bigint }> {
        const ntv = new ethers.Contract(
            L2_NATIVE_TOKEN_VAULT_ADDRESS,
            [...NativeTokenVaultAbi, ...NTV_EXTRA_ABI],
            this.provider
        );
        const token: string = await ntv.tokenAddress(assetId);
        const balance: bigint = await new ethers.Contract(token, ERC20Abi, this.provider).balanceOf(this.address);
        return { token, balance };
    }
}

// --- Flow -------------------------------------------------------------------------------------------

async function waitFor<T>(what: string, fn: () => Promise<T | null>): Promise<T> {
    const deadline = Date.now() + POLL_TIMEOUT_MS;
    while (Date.now() < deadline) {
        const result = await fn();
        if (result !== null) return result;
        await new Promise((resolve) => setTimeout(resolve, POLL_INTERVAL_MS));
    }
    throw new Error(`timed out waiting for ${what}`);
}

async function main() {
    const alice = new Participant(A);
    const bob = new Participant(B);

    console.log('=== SIGN IN ===');
    await alice.login();
    await bob.login();

    console.log('\n=== ON-CHAIN PREREQUISITES (each user, own chain) ===');
    const assetIdA = await alice.prepareToken();
    const assetIdB = await bob.prepareToken();

    console.log('\n=== 1. PROPOSE (Alice, instance A) ===');
    // Alice pays Bob on chain B; Bob pays Alice on chain A. proposalId, deadline and settlement chain
    // are filled in by the instance — they are never accepted from the body.
    const created = await alice.api<Proposal>('POST', '/interop/proposals', {
        legs: [
            {
                sourceChainId: A.chainId.toString(),
                destinationChainId: B.chainId.toString(),
                assetId: assetIdA,
                amount: AMOUNT.toString(),
                recipient: bob.address,
                senderAddress: alice.address
            },
            {
                sourceChainId: B.chainId.toString(),
                destinationChainId: A.chainId.toString(),
                assetId: assetIdB,
                amount: AMOUNT.toString(),
                recipient: alice.address,
                // Required on every leg, including the counterparty's: a proposal fully specifies who
                // funds and commits what, and this address binds that leg's salt as its msg.sender.
                senderAddress: bob.address
            }
        ]
    });
    const id = created.proposalId;
    console.log(`proposal ${id} created (status ${created.status})`);

    console.log('\n=== 2. APPROVE (each user on their own instance) ===');
    // Approval is per leg, and each party approves only the leg they fund: Alice leg 0, Bob leg 1.
    await alice.api('POST', `/interop/proposals/${id}/approve`, { legIndex: 0 });
    console.log('[Alice] approved leg 0');

    // Instance B only learns of the proposal once A's worker relays the terms, so it 404s until then.
    // There is nothing to claim: Bob owns leg 1 because it originates on his chain and names one of his
    // wallets as senderAddress, so the proposal is his the moment it lands.
    await waitFor('the terms to reach instance B', () => bob.find(id));
    await bob.api('POST', `/interop/proposals/${id}/approve`, { legIndex: 1 });
    console.log('[Bob] approved leg 1');

    console.log('\n=== 3. COMMIT (each user signs their own leg) ===');
    // The workers now build the legs, exchange hashes and bundles, and derive flowId. commit-params
    // stays empty until this instance has a built leg to commit.
    for (const participant of [alice, bob]) {
        const commits = await waitFor(`${participant.side.label}'s commit params`, async () => {
            const res = await participant.api<{ commits: CommitTx[] }>(
                'GET',
                `/interop/proposals/${id}/commit-params`
            );
            return res.commits.length > 0 ? res.commits : null;
        });
        await participant.commit(commits);
    }

    console.log('\n=== 4. WATCH (workers prove, wait for roots, execute) ===');
    // Poll BOTH instances: status is per-instance, and each executes only its own incoming leg. One
    // side reporting 'completed' says nothing about the other — an instance whose operator cannot pay
    // gas retries indefinitely, so trusting a single view reports success while a party is unpaid.
    const final = await waitFor('both instances to complete the flow', async () => {
        const seen = await Promise.all(
            [alice, bob].map(async (p) => ({ label: p.side.label, proposal: await p.find(id) }))
        );
        for (const { label, proposal } of seen) {
            if (!proposal) throw new Error(`[${label}] lost sight of proposal ${id}`);
            if (proposal.status === 'aborted' || proposal.status === 'refunded') {
                throw new Error(`[${label}] flow ended in status '${proposal.status}'`);
            }
        }
        console.log(`  ${seen.map((s) => `${s.label}: ${s.proposal!.status}`).join('   ')}`);
        return seen.every((s) => s.proposal!.status === 'completed') ? seen[0].proposal : null;
    });
    console.log(`flow ${final.flowId} completed on both instances`);

    console.log('\n=== VERIFY MINTS ===');
    for (const [receiver, assetId, from] of [
        [bob, assetIdA, 'Alice'],
        [alice, assetIdB, 'Bob']
    ] as const) {
        const { token, balance } = await receiver.wrappedBalance(assetId);
        console.log(`[${receiver.side.label}] holds ${ethers.formatUnits(balance, 18)} of ${from}'s asset (${token})`);
        if (balance < AMOUNT) {
            throw new Error(
                `${receiver.side.label} received ${ethers.formatUnits(balance, 18)}, expected ${ethers.formatUnits(AMOUNT, 18)}`
            );
        }
    }

    console.log('\nAtomic swap complete — driven entirely through the Prividium interop API.');
}

main().catch((err) => {
    console.error('Error:', err instanceof Error ? err.message : err);
    process.exit(1);
});
