# Picore

Find and play Apple Music animated album and playlist artwork. No user account is required.

## Run

```sh
docker compose up -d --build
```

Open http://localhost:3000. The SQLite cache is persisted in a Docker volume. Alternatively, run `cargo run --release` with Rust 1.96 or newer and FFmpeg installed on the server. Set `BIND_ADDRESS` (default `0.0.0.0:3000`) and `DATABASE_PATH` (default `cache.db`) as needed.

## API

- `GET /api/v1/artwork/url?url=<Apple Music URL>`
- `GET /api/v1/artwork/search?artist=<artist>&album=<album>`
- `GET /api/v1/artwork/download?url=<returned HLS URL>&format=mp4` (or `webp`)
- `GET /api/v1/artwork/history`
- `GET /api/v1/status`

Artwork responses include `url` (square), `url_tall`, `artist`, `album`, and `isCached`. URLs are HLS playlists; playback requires an HLS-capable player. Search matches artist and album names against iTunes catalog results, preferring exact albums over editions. Successful results are cached for seven days; albums without animation and confirmed search misses are cached for one day. Apple Music's public web catalog is an unofficial upstream and can change or rate-limit requests.

Downloads convert a previously looked-up stream on the server (up to 60 seconds, 64 MiB and two concurrent conversions).
