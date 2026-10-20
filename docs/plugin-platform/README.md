# TokenMe plugin platform design

Design work on branch plug. Production integration is not implemented; the standalone alpha reference supports only offline quota.read and usage.collect.

- [Architecture and scope](PLAN.md)
- [Implementation tickets and developer workflow](IMPLEMENTATION_SPEC.md)
- [Production contracts](PRODUCTION_CONTRACTS.md) and [design hardening](DESIGN_HARDENING.md)
- [Language and SDK](LANGUAGE_AND_SDK.md), [logging and diagnostics](LOGGING_DIAGNOSTICS.md), [migration inventory](MIGRATION_INVENTORY.md)
- [Latest adversarial audit](AUDIT_ROUND2.md) and [executable design checks](review/README.md)
- [Agent gateway extension](AGENT_GATEWAY.md), [gateway versus delegation assessment](AGENT_MODES_EVALUATION.md), [WorkBuddy and Claude Code scenario](WORKBUDDY_CLAUDE_CODE.md)
- [Runnable reference kit](../../prototypes/plugin-platform-v1/README.md)

The design documents are now versioned here. Internal working copies, original audit logs, machine-specific discovery records, build outputs and development profiles remain local and ignored. Historical run identifiers and digests describe their recorded runs; reproduce checks with the source commands, rather than treating unavailable local artifacts as checked-in evidence. Gateway and delegation require their own schema freeze and real-service acceptance.
