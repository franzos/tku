# Security Policy

tku reads local usage data and talks to provider APIs, so I'd rather hear about
security issues privately. Thanks for taking the time to look.

## Supported versions

This is pre-1.0 software and moves fast. Only the latest tagged release and the
`master` branch get security fixes. There are no backports to older `0.x` tags,
so if you're running an older build, the fix is to upgrade.

| Version        | Supported |
| -------------- | --------- |
| latest release | yes       |
| `master`       | yes       |
| older `0.x`    | no        |

## Reporting a vulnerability

Please report privately, not through a public issue or pull request.

- Email: mail@gofranz.com
- If you use GitHub, you can also open a private advisory via the repository's
  Security tab ("Report a vulnerability").

Useful things to include, as far as you have them:

- what the issue is and the impact you think it has
- the affected version or commit
- steps to reproduce, or a proof of concept
- any logs or config (with secrets redacted) that help me confirm it

## What to expect

I'll acknowledge your report, confirm whether I can reproduce it, and keep you
updated as I work on a fix. Once it's resolved I'm happy to credit you in the
release notes, or keep you anonymous if you'd rather. Please give me a chance to
ship a fix before disclosing publicly.

## Scope

The tku codebase in this repository is in scope. Issues in third-party
dependencies are better reported upstream, though I do want to hear about it if a
dependency issue is exploitable through how tku uses it.
