# Public-release checklist

Run this checklist before making the repository public or pushing a new public
history. The application is intended to be open source, but local dictation
data is not source material.

- Review `git status --short`, `git ls-files`, and all untracked files.
- Search source, documentation, fixtures, and examples for names, email
  addresses, usernames, hostnames, device identifiers, absolute local paths,
  private vocabulary, transcript excerpts, and local benchmark data.
- Search for credentials and credential-shaped values: API keys, bearer tokens,
  cookies, passwords, private URLs, and provider configuration.
- Confirm that recordings, histories, model files, evaluation datasets, logs,
  caches, and customized configuration files are absent.
- Inspect commit metadata and remotes separately from source contents. Author
  names, email addresses, signed commits, branch names, and remote URLs can
  disclose information even when the working tree is clean.
- Choose and add an explicit open-source license before publishing. Do not
  assume a license without deciding which terms are appropriate.
- Run the checks in `README.md`, then perform a final diff review from a clean
  checkout or staging area.

The `.gitignore` file is a safety net, not a privacy guarantee. Review every
file and every commit that will be visible to the public.
