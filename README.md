# GridCraft for Taskforce: rebuild pipeline

This branch holds no GridCraft source. It holds the workflow that rebuilds
[storytold/gridcraft](https://github.com/storytold/gridcraft) with the Taskforce patches
(branch `tf/stack`) for every upstream release, and the gate each build has to pass.

- `.github/workflows/tf-release.yml`: every six hours, re-apply `tf/stack` on the latest
  upstream tag, test, build, run the gate, publish `gridcraft-web-<version>-tf.<n>.zip`.
- `gate/gate.mjs`: the browser gate (host bridge, co-editing protocol, browser and native
  parity on a fixed command script).

When a patch stops applying or a check fails, the run fails and nothing is published.
Fix `tf/stack` (rebase it on the new tag, resolve, push) and run the workflow again.

Run the gate by hand:

```sh
cd gate && npm ci
CHROME_PATH=/path/to/chrome node gate.mjs --dist <web build dir> --cli <gridcraft-cli>
```
