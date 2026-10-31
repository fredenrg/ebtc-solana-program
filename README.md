# eBTC on Solana — reward engine program

On-chain program behind [eBTC](https://ebtc.fredenergy.org): Eco Bitcoin Virtual Miner NFTs,
tier-weighted eBTC reward draws, and automatic liquidity routing of NFT sales.

| | Mainnet address |
|---|---|
| Program | `7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc` |
| eBTC token mint (mint authority revoked) | `FbF23z2ra7DEsqVu2LEPWhAQCmEtCc5uLvVibKzVHLoy` |
| NFT collection (Metaplex Core) | `7XzbxNfz4dfosVLsQSkL991HR1TSLUMr3ZzF45M2eu4` |
| eBTC/SOL pool (Raydium CPMM) | `2Q8uaDj1dj1uc8KeZXeD3MPnYuMYxJ7te7xb8TMWmTZh` |

The program's IDL is published on-chain (`anchor idl fetch 7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc`).

## What it does

- **Public mint** (`mint_nft`): five tiers of Metaplex Core NFTs at fixed SOL prices. Each NFT's
  name and metadata URI are proven against a per-tier Merkle root committed on-chain.
- **Reward draws**: one draw every 10 minutes using MagicBlock VRF. Winners are selected by tier
  weight from paged on-chain buckets and paid from a PDA-owned eBTC vault, with per-tier reward
  multipliers and a per-draw cap.
- **Liquidity routing**: 95% of each sale goes to a PDA vault and is routed into the Raydium pool
  (`execute_route`: swap up to 49% for eBTC, then deposit). LP tokens are held by a
  program-owned account; there is no withdraw instruction.
- **Legacy migration** (`migrate_mint`): NFTs migrated from Stellar keep their reserved token ids,
  which public mints cannot use.

## Verified build

The deployed program is a [verifiable build](https://solana.com/docs/programs/verified-builds),
verified by [OtterSec](https://verify.osec.io/status/7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc).
It is built in Docker with the Solana 2.1.0 image; other images (including the `solana-verify`
default) produce a different binary.

```bash
cargo install solana-verify
solana-verify build --library-name reward_engine \
  --base-image solanafoundation/solana-verifiable-build:2.1.0
solana-verify get-executable-hash target/deploy/reward_engine.so
solana-verify get-program-hash -um 7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc
```

Both hashes should be `6bee2faf4b125d60a65387361b9b62b65e1be3c9c446f07547a083390bcd061a`.

To verify against this repository directly:

```bash
solana-verify verify-from-repo -um \
  --program-id 7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc \
  https://github.com/fredenrg/ebtc-solana-program \
  --library-name reward_engine \
  --base-image solanafoundation/solana-verifiable-build:2.1.0
```

For local development, `anchor build` (Rust per `rust-toolchain.toml`, Solana CLI, Anchor 0.31.1)
also works, but its output is host-dependent and will not match the on-chain hash.

## Security

Please report vulnerabilities privately to `hello@fredenergy.org`
(see [security.txt](https://ebtc.fredenergy.org/.well-known/security.txt)).

## License

[Business Source License 1.1](LICENSE). You may read, audit, build and verify this code; production
use requires a commercial licence until the Change Date, when it converts to GPL-2.0-or-later.
