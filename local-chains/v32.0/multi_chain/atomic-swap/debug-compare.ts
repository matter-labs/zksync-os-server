/**
 * Debug: compare `buildBundle`'s reconstruction against the bundle the contract actually emits.
 * Sends one real (non-atomic) bundle so the `InteropBundleSent` event gives ground truth, then
 * decodes both and prints the first differing field.
 */
import { ethers } from 'ethers';
import {
  BundleBuilder,
  CallBuilder,
  L2_NATIVE_TOKEN_VAULT_ADDRESS,
  NativeTokenVaultAbi,
  ERC20Abi,
  resolveAtomicLayout,
  AtomicInteropCenterAbi,
  INTEROP_BUNDLE_TUPLE,
  interopBundleSaltAttr,
  randomSalt,
} from './lib';
import { buildBundle } from './lib/build-bundle';

const TOKEN_BYTECODE_FILE = './atomic-swap-3chains.ts';

async function main() {
  const layout = resolveAtomicLayout();
  const provider = new ethers.JsonRpcProvider(process.env.L2_RPC_URL!);
  const wallet = new ethers.Wallet(process.env.PRIVATE_KEY!, provider);
  const chainId = (await provider.getNetwork()).chainId;
  const destChainId = BigInt(process.env.DEST_CHAIN_ID!);
  const amount = ethers.parseUnits('1', 18);

  // Reuse an already-deployed+approved token if given, else deploy one.
  let token = process.env.TOKEN ?? '';
  if (!token) {
    const src = require('fs').readFileSync(TOKEN_BYTECODE_FILE, 'utf8');
    const bytecode = /const TOKEN_BYTECODE =\s*'([^']+)'/.exec(src)![1];
    const ctorArgs = ethers.AbiCoder.defaultAbiCoder().encode(['uint256'], [ethers.parseUnits('1000000', 18)]);
    const dep = await wallet.sendTransaction({ data: bytecode + ctorArgs.substring(2) });
    token = (await dep.wait())!.contractAddress!;
    const ntv = new ethers.Contract(
      L2_NATIVE_TOKEN_VAULT_ADDRESS,
      [...NativeTokenVaultAbi, 'function ensureTokenIsRegistered(address _nativeToken) returns (bytes32)'],
      wallet
    );
    await (await ntv.ensureTokenIsRegistered(token)).wait();
    await (await new ethers.Contract(token, ERC20Abi, wallet).approve(L2_NATIVE_TOKEN_VAULT_ADDRESS, amount * 10n)).wait();
  }
  console.log('token:', token);

  const salt = randomSalt();
  const starter = CallBuilder.tokenTransfer(chainId, token, amount, wallet.address);

  // 1) local reconstruction (assertion disabled so we can inspect the difference)
  const built = await buildBundle({
    provider,
    sender: wallet.address,
    destinationChainId: destChainId,
    salt,
    calls: [{ to: L2_ASSET_ROUTER(), data: starter.data, indirectCall: true }],
    interopCenter: layout.interopCenter,
    verifyAgainstChain: false,
  });

  // 2) ground truth: send for real and read the event
  const ic = new ethers.Contract(layout.interopCenter, AtomicInteropCenterAbi, wallet);
  const fee: bigint = await ic.interopProtocolFee();
  const builder = new BundleBuilder(destChainId).addCall(starter);
  const tx = await ic.sendBundle(builder.getEncodedDestination(), builder.getCalls(), [interopBundleSaltAttr(salt)], {
    value: fee,
  });
  const receipt = await tx.wait();
  const iface = new ethers.Interface(AtomicInteropCenterAbi);
  let real = '';
  for (const log of receipt!.logs) {
    if (log.address.toLowerCase() !== layout.interopCenter.toLowerCase()) continue;
    const parsed = iface.parseLog({ topics: log.topics as string[], data: log.data });
    if (parsed?.name === 'InteropBundleSent') {
      real = ethers.AbiCoder.defaultAbiCoder().encode([INTEROP_BUNDLE_TUPLE], [parsed.args.interopBundle]);
      break;
    }
  }
  if (!real) throw new Error('no InteropBundleSent event');

  console.log('\nmatch:', built.bundleData === real);
  const [b] = ethers.AbiCoder.defaultAbiCoder().decode([INTEROP_BUNDLE_TUPLE], built.bundleData);
  const [r] = ethers.AbiCoder.defaultAbiCoder().decode([INTEROP_BUNDLE_TUPLE], real);
  const names = ['version', 'sourceChainId', 'destinationChainId', 'destBaseTokenAssetId', 'interopBundleSalt'];
  for (let i = 0; i < names.length; i++) {
    if (String(b[i]) !== String(r[i])) console.log(`DIFF ${names[i]}:\n  built ${b[i]}\n  real  ${r[i]}`);
  }
  for (let i = 0; i < Math.max(b[5].length, r[5].length); i++) {
    const bc = b[5][i], rc = r[5][i];
    const f = ['version', 'shadowAccount', 'to', 'from', 'value', 'data'];
    for (let j = 0; j < f.length; j++) {
      if (String(bc?.[j]) !== String(rc?.[j])) console.log(`DIFF call[${i}].${f[j]}:\n  built ${bc?.[j]}\n  real  ${rc?.[j]}`);
    }
  }
  const f2 = ['executionAddress', 'unbundlerAddress', 'useFixedFee', 'salt'];
  for (let j = 0; j < f2.length; j++) {
    if (String(b[6][j]) !== String(r[6][j])) console.log(`DIFF attrs.${f2[j]}:\n  built ${b[6][j]}\n  real  ${r[6][j]}`);
  }
}

function L2_ASSET_ROUTER(): string {
  return '0x0000000000000000000000000000000000010003';
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
