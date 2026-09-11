# Community form registry

Forms listed here get checked automatically on a schedule (see
`.github/workflows/monitor.yml`), with results published to the
[dashboard](../site/index.html) — see
[docs/community-design.md](../docs/community-design.md) for the full
design and why it's built this way (no server, no database).

## Adding a form

1. Pick (or create) a jurisdiction folder — e.g. `us-federal/`,
   `us-state-ca/`, `us-city-sf/`. Use whatever grouping makes sense; this
   is about keeping PRs reviewable, not a rigid taxonomy.
2. Add a `.yml` file in it, one form per file, in the same format as a
   local `forms.yml`:
   ```yaml
   forms:
     - name: Business License Renewal
       url: https://city.gov/business-license
   ```
3. Open a PR. Review is just "is this a real public-service form, does
   the YAML parse" — not a code review.

## What NOT to add

- Anything that isn't a genuine public-service form (this registry
  exists to hold the maintainers and the community accountable to *real*
  civic infrastructure, not to be a general-purpose site-checker).
- A form that requires login/authentication to reach — formwatch can't
  authenticate, and testing a form behind auth risks touching a real
  account's data.
- Anything you don't have a good-faith belief is fine to run automated,
  no-`--submit` traffic against on a recurring schedule. If you're
  unsure, ask in the PR before adding it.

`example/example-form.yml` is a placeholder, not a real form — replace
or remove it, don't leave it mixed in with genuine entries.
