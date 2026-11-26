# RADE V2 text channel

`pipeline/` is copied **unmodified** from
<https://github.com/freedv/freedv-backend>, branch `radev2-dev`, commit
`20628fe221a987ba638e55d036dce92884737508` (`src/pipeline/`). BSD-2-Clause, see
`LICENSE`.

It is the protocol that carries the callsign in RADE V2's data symbols (one
bit per modem frame). V2 is still under development, so this is taken as it is
rather than ported: updating it means copying the files again, together with a
matching `vendor/rade_c`.

`util/logging/ulog.h` is ours: it stands in for FreeDV's logger.
