# get_status

Effect: read. Response class: ack (2 KiB cap). Idempotent: yes.

Reports the product identity and the release qualification recorded in the
embedded family manifest (`not_verified` when the manifest is unreadable).
Read-only: no files, no external services.

## Arguments

None; an empty argument object is required. Any argument is refused with
`ERROR invalid_arguments:` and `isError=true`.

## Reply

`OK <product> <version>` plus the `Release qualification:` line. The declared
release qualification is unchanged by this reply; it is reported, never
asserted.

## Structured content

Every reply also carries MCP `structuredContent` conforming to the tool's
`outputSchema` (exported in `schemas/tools.json`): the outer envelope is
`{"schema_version":1,"kind":"identity","data":{product,version,qualification}}`
on success and
`{"schema_version":1,"kind":"identity","error":{code,outcome,detail}}` for a
refused call — never an empty success. The exact product and package version
are compile-time constants, so the structured identity exists even when the
text presentation degrades. The one text block is not a JSON duplicate.
