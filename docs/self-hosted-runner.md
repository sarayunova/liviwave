# Self-hosted CI runner

CI (`.github/workflows/ci.yml`) runs `cargo xtask ci` on a self-hosted GitHub Actions runner with the
label `liviwave-local`, so it uses no GitHub-hosted minutes.

## One-time setup (Windows)

1. Download the runner from <https://github.com/actions/runner/releases> into its own folder
   outside the repository and unzip it.
2. Create a registration token: `gh api -X POST repos/<owner>/liviwave/actions/runners/registration-token --jq .token`.
3. `config.cmd --url https://github.com/<owner>/liviwave --token <token> --name liviwave-local --labels liviwave-local --unattended --replace`
4. In the runner folder, create `.env` with:
   ```
   CARGO_TARGET_DIR=<a persistent folder for the build cache>
   LIVIWAVE_VCVARS=<path to vcvars64.bat of Visual Studio Build Tools>
   ```

## Each session

Run `run.cmd` in the runner folder (or install it as a service with `svc install` if you want it
always on). While the runner is offline, a CI run waits in the queue; stop the runner to pause CI.

The runner executes this repository's workflow code on your machine, so keep the repository private
and don't run pull requests from people you don't trust.
