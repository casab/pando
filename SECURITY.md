# Security policy

## Reporting a vulnerability

Please report security problems privately, never in a public issue,
pull request or discussion.

Use GitHub's private vulnerability reporting: the **Report a
vulnerability** button on the repository's
[Security tab](https://github.com/mertkaradayi/pando/security/advisories/new).
If that is not available to you, email the maintainer at
imertkaradayi@gmail.com with `pando security` in the subject.

Include what you found, the version (`pando --version`), your OS, and the
steps that show it. You will get an answer within a week. Once a fix is
out, the advisory is published with credit to you unless you would
rather not be named.

## Supported versions

pando is pre-1.0. Security fixes go into the latest version only.

## What is in scope

pando runs commands on a developer's machine and reaches into servers
they own, so these are the areas that matter most:

- **Writing where it should not.** pando promises never to write into a
  repository; everything lives under `~/.pando`. A way to make it write
  elsewhere, or follow a link out of its own tree, is in scope.
- **Dropping data it did not make.** Namespaced mode makes and drops
  databases and Redis slots in the developer's own servers. Anything that
  gets `rm`, `doctor` or a start to drop, flush or overwrite data pando
  did not create is in scope.
- **Credentials.** Database logins pando keeps are stored with mode 0600
  and handed to clients through the environment, never a command line. A
  leak of a secret into logs, `doctor`, `signals`, JSON output, a shell
  history or another user's reach is in scope.
- **Command injection** through a branch name, a worktree name, a pull
  request's title or branch, a config value or a recipe.
- **Sharing.** `share` exposes a running worktree at a public URL. Anything
  that exposes more than the worktree the developer chose, or keeps it
  exposed after `unshare` or `stop`, is in scope.

A project's own `pando.toml`, recipes and hooks are code the developer
chose to run, the same as a `Makefile`; that they run commands is not a
vulnerability.
