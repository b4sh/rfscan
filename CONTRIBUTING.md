# Contributing to rfscan

Bug reports, reports about receivers, ideas and patches are welcome.

## Reporting a problem

Open an issue with:

- the rfscan version (`rfscan -V`) and the system (Windows version, Linux distribution);
- the receiver and the output of `rfscan --list-devices`;
- the full command line (and your `rfscan.conf`, if you use one);
- what happened and what you expected; the console output of a run with `--headless --log FILE --log-level debug` helps a lot.

## Receivers

If rfscan works (or does not) with a receiver that is not in the table in [README.md](README.md#tested-receivers), please report the settings you used (`--sample-rate`, `--gain`, `--stream-args`, the `--settle` suggested by `rfscan --calibrate`) and the scan speed from `rfscan --bench`.

## Patches

- One change per pull request, with a short description of what and why.
- `cargo build --release`, `cargo clippy` and `cargo fmt` without warnings, `cargo test` passing.
- No hard-coded limits of a particular receiver: ranges, sample rates, gains and formats come from SoapySDR.
- No allocation in the scan and DSP loops.
- Code, comments and messages in English.

By contributing you agree that your contribution is licensed under the same license as rfscan, AGPL-3.0-or-later.
