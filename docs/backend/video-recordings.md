# Camera video recordings

Ground Station's MediaMTX configuration records every published camera stream to
disk automatically. These are the **original encoded video/audio tracks**, remuxed
into fragmented MP4 without transcoding. They are not uncompressed sensor frames.
The browser voice channel is separate and is not recorded.

Each camera gets its own directory and timestamped MP4 segments, normally about
one minute long. Segment boundaries depend on incoming keyframes. One-second
parts limit how much of the active file can be lost on an abrupt shutdown.
Already-written fragments can remain usable after an interruption. See the
[MediaMTX recording documentation](https://mediamtx.org/docs/features/record).

## Storage and deployment

- Native/systemd default: `backend/data/video_recordings/STREAM/TIMESTAMP.mp4`,
  relative to the backend service's working directory.
- Native override: set `GS_VIDEO_RECORDINGS_DIR` on the Ground Station service;
  the managed MediaMTX child receives the same directory automatically.
- Docker: the video overlay mounts `./backend/data/video_recordings` into MediaMTX
  at `/recordings` and exposes the same files to the backend's existing data mount.
- Separately managed MediaMTX: use the updated `backend/config/mediamtx.yml` and
  make its recording directory accessible to the backend. Restart the receiver
  to apply the new recording configuration.

Rebuild/restart the native backend, or recreate the Docker video overlay, to enable
recording with these settings. The service user needs write access to its recording
directory. Existing streams will start producing segments after the new receiver
configuration loads; earlier video cannot be recovered retroactively.

**Files are retained until manually removed.** Automatic deletion is disabled
(`recordDeleteAfter: 0s`). At 2 Mbit/s, one camera uses roughly 0.9 GB per hour,
plus container/audio overhead. Monitor available disk space and archive or remove
old completed segments through your normal filesystem tools. Do not remove files
being written. There is no delete button or automatic quota enforcement in this UI.

## Review and download

Open **Camera recordings** from Mission, or go to `/media#recordings-heading` on
the Ground Station's HTTPS origin. Sign in on the media page with operator or
stream-manager access. Recordings are protected like live previews so they cannot
bypass the audience's delayed-view permissions.

The list shows the camera, timestamped filename, and file size, newest first, with
50 files per page. Timestamps in filenames use the receiver's local time. Select
**Refresh recordings** for newly completed segments. Files modified in the last
five seconds are marked as updating and temporarily unavailable for playback.
This is a settling check, not a guarantee that a publisher can never resume a file.

**Play** opens the original MP4 in the page. **Download original** saves it without
loading the entire file into browser memory. Downloads/playback support HTTP byte
ranges. If the camera uses a codec unsupported by the browser, download the file
and use a compatible player. Access links expire; refresh the list to renew them.

## API

`GET /api/video/recordings?offset=0&limit=50` requires a bearer session with live
preview access and returns `recordings`, `total`, `offset`, and `limit` (maximum 100).
Each recording has `stream`, `file`, `size_bytes`, `modified_ms`,
`recently_updated`, and a resource-scoped `url`.

`GET /api/media-assets/recordings/STREAM/FILE?ticket=...` serves the original MP4.
Its ticket is scoped to that file and checked against current session access on
every request. Recently modified files return 409. Paths are validated and
symlinks are rejected. Responses use private/no-store caching.
