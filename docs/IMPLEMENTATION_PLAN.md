# Product acceptance plan

1. Review and commit the generated Cargo.lock. Run cargo xtask check and focused protocol/presentation/delivery tests.
2. Replace generated not_implemented tools with real typed contracts, outcomes, MiniJinja views and failure/recovery tests.
3. Validate every declared profile and the actual supported host. SDK-pair tests are not real-host qualification.
4. Test local package/install/no-op/corruption/activation on macOS arm64 in disposable directories.
5. Configure release environment review, record native/host evidence, then deliberately enable publishing.
6. Test a disposable real release and its observer before shipping. Never claim a hash is a signature or a process exit is an agent turn.
7. Apply template update plans through normal reviewed Git changes. No unsafe automatic overwrite.

The starter does not need a daemon, database or external configuration. Add them only for actual product requirements.
