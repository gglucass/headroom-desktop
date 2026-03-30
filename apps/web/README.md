# Headroom Web

First Rails version of the Headroom website.

## Current pages

- `/`: app landing page with download CTA

## Run locally

```bash
bundle install
bin/rails db:migrate
bin/rails server
```

Open http://localhost:3000.

## Download link configuration

Set `HEADROOM_MAC_DOWNLOAD_URL` to enable the primary download button:

```bash
HEADROOM_MAC_DOWNLOAD_URL="https://example.com/headroom-desktop.dmg" bin/rails server
```

## Production launch checklist

Required environment variables:

- `RAILS_ENV=production`
- `RAILS_MASTER_KEY` (value from `config/master.key`)
- `SECRET_KEY_BASE` (random long secret)
- `APP_HOST` (your domain, e.g. `headroomlabs.ai`)
- `HEADROOM_MAC_DOWNLOAD_URL` (download URL for the app)
- `DATABASE_URL` (recommended: managed Postgres URL; optional when using sqlite volume)
- `ASSUME_SSL=true` and `FORCE_SSL=true` when behind HTTPS proxy

Release commands:

```bash
bin/rails db:migrate
```

## Railway setup

This Rails app is ready to deploy to Railway from a monorepo.

Recommended service settings:

- Connect the GitHub repo that contains this `apps/web` directory.
- Set the trigger branch to `main`.
- Set the root directory to `/apps/web`.
- Set the config-as-code path to `/apps/web/railway.json`.
- Leave auto deploy enabled.

The checked-in Railway config:

- builds with the app's `Dockerfile`
- waits for `GET /up` to return `200` before marking a deploy healthy

Required Railway variables:

- `RAILS_MASTER_KEY`
- `SECRET_KEY_BASE`
- `APP_HOST`
- `HEADROOM_MAC_DOWNLOAD_URL`
- `DATABASE_URL` if you are using Postgres instead of the default sqlite storage
- `ASSUME_SSL=true` and `FORCE_SSL=true` when running behind Railway's HTTPS proxy

After the first successful deploy, pushing new commits to `main` should trigger fresh Railway deployments automatically.

If you prefer GitHub Actions-driven deploys instead of Railway's repo integration, this repo also includes `.github/workflows/deploy-web-railway.yml`. That workflow deploys `apps/web` to the `headroom-web` Railway project on every push to `main` and needs one GitHub secret:

- `RAILWAY_TOKEN` as a Railway project token scoped to `headroom-web`
