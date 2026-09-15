# Squads IDLs

These IDLs are pinned inputs for the generated Carbon multisig decoders. They
come from the official Squads Protocol repositories listed below.

| File | Upstream source | Commit | Program ID | IDL version |
| --- | --- | --- | --- | --- |
| `v3.json` | `Squads-Protocol/squads-mpl/sdk/lib/idl/squads_mpl.json` | `3440b7435bd3a57b473b69477e42aa51455e48ad` | `SMPLecH534NA9acpos4G6x7uf3LWbCAwZQE9e8ZekMu` | `1.3.0` |
| `v4.json` | `Squads-Protocol/v4/sdk/multisig/idl/squads_multisig_program.json` | `c14a6d607a4d5295c7df90499674418b57aff792` | `SQDS4ep65T869zMMBKyuUq6aD6EgTu8psMjkvj52pCf` | `2.1.0` |
| `smart-account-v0.1.json` | `Squads-Protocol/smart-account-program/sdk/smart-account/idl/squads_smart_account_program.json` | `80bf1f7ad28fd1176c364879776982730b8e9c80` | `SMRTzfY6DfH5ik3TKiyLFfXexV8uSG3d2UksSCYdunG` | `0.1.0` |

The IDL version is also hardcoded as the emitted `idl_version` label in
`crates/microscope-indexer/src/multisig/{v3,v4,smart_account}.rs`; update
those constants when re-pinning an IDL.

The Smart Account SDK copy is intentional. The repository-root IDL is stale
and omits types required by the current program interface.

Squads announced this Smart Account generation as Protocol v5, while the
deployed repository and IDL label the interface as v0.1. Microscope preserves
both identifiers as `squads_version = "v5"` and `idl_version = "0.1.0"`.
