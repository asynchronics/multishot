# 0.3.1 (2024-03-19)

- Replace `executor` crate with `pollster` in doctests to work around possible
  soundness issue in `executor` crate ([#2]).
- Include doctests in Miri CI workflow.
- Silence new warnings when running tests with nightly.
- Update copyright date in MIT license.

Note: this release does not contain any functional change; upgrading from 0.3.0
is not mandated.

[#2]: https://github.com/asynchronics/multishot/pull/2

# 0.3.0 (2022-12-29)

- Implement `Send`, `UnwindSafe` and `RefUnwindSafe` for `Sender`.
- Simplify implementation and improve performance.
- Enable multi-threading in MIRI tests and add tests.

# 0.2.0 (2022-02-14)

- Fix soundness issue in `Receiver::sender` which should have taken `&mut self`
  rather than `&self`.
- Update copyright date in MIT license.

# 0.1.0 (2022-02-10)

Initial release
