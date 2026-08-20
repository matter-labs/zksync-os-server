/**
 * Deploys one ERC20 per side and prints the TOKEN_A/TOKEN_B env line.
 *
 * The prividium driver takes tokens as input rather than deploying them (each party only ever
 * touches its own chain), and chain state is ephemeral — so this has to be re-run after every
 * `run_local.sh` restart.
 */
import { ethers } from 'ethers';
import * as fs from 'fs';

const SUPPLY = ethers.parseUnits('1000000', 18);

// Keys must match atomic-swap-prividium.ts exactly — a token deployed to a different holder than the
// one the swap runs as leaves the sender with a zero balance, which only surfaces later as a failed
// leg build. Alice is the seed's "Test User", not the admin wallet (that one is the interop operator).
const ALICE_KEY = process.env.ALICE_PRIVATE_KEY ?? '0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a';
const BOB_KEY = process.env.BOB_PRIVATE_KEY ?? '0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d';

const SIDES = [
  { label: 'TOKEN_A', rpc: process.env.A_RPC_URL ?? 'http://127.0.0.1:3050', key: ALICE_KEY },
  { label: 'TOKEN_B', rpc: process.env.B_RPC_URL ?? 'http://127.0.0.1:3051', key: BOB_KEY },
];

async function main() {
  const src = fs.readFileSync(`${__dirname}/atomic-swap-3chains.ts`, 'utf8');
  const bytecode = /const TOKEN_BYTECODE =\s*'([^']+)'/.exec(src)![1];
  const ctorArgs = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [SUPPLY]);

  const out: string[] = [];
  for (const side of SIDES) {
    const wallet = new ethers.Wallet(side.key, new ethers.JsonRpcProvider(side.rpc));
    const receipt = (await (await wallet.sendTransaction({ data: bytecode + ctorArgs.substring(2) })).wait())!;
    console.log(`${side.label}: ${receipt.contractAddress} (owner ${wallet.address}, ${side.rpc})`);
    out.push(`${side.label}=${receipt.contractAddress}`);
  }
  console.log(`\n${out.join(' ')} npx ts-node atomic-swap-prividium.ts`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
