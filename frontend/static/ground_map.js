//
// frontend/assets/ground_map.js
//
// Leaflet helpers for GroundStation26
// - ES module with named exports (required by wasm-bindgen)
// - Emoji-based markers (🚀 + 🧍) with heading indicator (▲)
// - Browser compass + geolocation support
//

let groundMap = null;
let groundTileLayer = null;
let rocketMarker = null;
let userMarker = null;
let rocketGuideLine = null;

// Remember last-known positions across tab switches
let lastRocketLatLng = null;
let lastUserLatLng = null;
let lastMapView = null;
let currentTilesUrl = null;
let currentMaxNativeZoom = null;
let currentMaxZoom = null;

// you currently have tiles for z = 0..8
const MIN_ZOOM = 0;
const DEFAULT_MAX_NATIVE_ZOOM = 12;
const DEFAULT_MAX_OVERZOOM_DELTA = 1;

// Must match Rust NA_BOUNDS in build.rs
const NA_BOUNDS = {
    lonMin: -170.0,
    latMin: 5.0,
    lonMax: -50.0,
    latMax: 83.0,
};

// =========================
// Marker sizing (TUNE HERE)
// =========================
const MARKER_PX = 48;                 // overall icon box
const MARKER_ANCHOR = MARKER_PX / 2;  // center anchor
const USER_FONT_PX = 25;              // 🧍 size
const ROCKET_FONT_PX = 25;            // 🚀 size
const ARROW_FONT_PX = 18;             // ▲ size
const ARROW_RADIUS = Math.round(MARKER_PX * 0.5) - 1; // ▲ distance from center

// Raw + filtered heading (0..360, 0 = North)
let userHeadingDegRaw = null;
let userHeadingDeg = null;
let nativeHeadingDeg = null;
let deviceHeadingDeg = null;
let compassInitialized = false;

// ============================================================================
// Utilities
// ============================================================================

function getLeaflet() {
    if (typeof L === "undefined") {
        throw new Error(
            "Leaflet global `L` is not defined. Load leaflet.js before wasm."
        );
    }
    return L;
}

function normalizeAngle(deg) {
    let d = deg % 360;
    if (d < 0) d += 360;
    return d;
}

function shortestAngleDiff(a, b) {
    let diff = normalizeAngle(b) - normalizeAngle(a);
    if (diff > 180) diff -= 360;
    if (diff < -180) diff += 360;
    return diff;
}

function circularMeanDeg(a, b, wa, wb) {
    const ar = normalizeAngle(a) * Math.PI / 180.0;
    const br = normalizeAngle(b) * Math.PI / 180.0;
    const x = Math.cos(ar) * wa + Math.cos(br) * wb;
    const y = Math.sin(ar) * wa + Math.sin(br) * wb;
    if (!Number.isFinite(x) || !Number.isFinite(y) || (x === 0 && y === 0)) {
        return normalizeAngle(a);
    }
    return normalizeAngle(Math.atan2(y, x) * 180.0 / Math.PI);
}

function fusedHeadingTarget() {
    const hasNative = Number.isFinite(nativeHeadingDeg);
    const hasDevice = Number.isFinite(deviceHeadingDeg);
    // Native mobile heading is already north-referenced and posture-independent.
    // Do not blend it with browser/deviceorientation data, which can vary with
    // screen posture and reintroduce orientation-dependent drift.
    if (hasNative) return normalizeAngle(nativeHeadingDeg);
    if (hasDevice) return normalizeAngle(deviceHeadingDeg);
    return null;
}

function applyFusedHeading() {
    const target = fusedHeadingTarget();
    if (!Number.isFinite(target)) return;

    userHeadingDegRaw = target;

    if (!Number.isFinite(userHeadingDeg)) {
        userHeadingDeg = target;
    } else {
        const diff = shortestAngleDiff(userHeadingDeg, target);
        const gain = Number.isFinite(nativeHeadingDeg)
            ? Math.min(0.92, Math.max(0.72, Math.abs(diff) / 45.0))
            : Math.min(0.55, Math.max(0.16, Math.abs(diff) / 90.0));
        userHeadingDeg = normalizeAngle(userHeadingDeg + diff * gain);
    }

    updateUserMarkerRotation();
}

// ============================================================================
// CSS injection (runs once)
// ============================================================================

function ensureMarkerStylesOnce() {
    if (document.getElementById("gs26-marker-styles")) return;

    const style = document.createElement("style");
    style.id = "gs26-marker-styles";
    style.textContent = `
    .user-marker-wrapper {
      position: relative;
      width: ${MARKER_PX}px;
      height: ${MARKER_PX}px;
      pointer-events: none;
    }

    .emoji-marker {
      position: absolute;
      left: 50%;
      top: 50%;
      transform: translate(-50%, -50%);
      line-height: 1;
      user-select: none;
      filter: drop-shadow(0 2px 2px rgba(0,0,0,0.55));
    }

    .user-base {
      font-size: ${USER_FONT_PX}px;
    }

    .rocket-marker {
      font-size: ${ROCKET_FONT_PX}px;
    }

    .user-heading-indicator {
      font-size: ${ARROW_FONT_PX}px;
      transform: translate(-50%, -50%) translateY(-${ARROW_RADIUS}px);
    }
  `;
    document.head.appendChild(style);
}

// ============================================================================
// Tile helpers
// ============================================================================

function clampMaxNativeZoom(value) {
    if (!Number.isFinite(value)) return DEFAULT_MAX_NATIVE_ZOOM;
    const z = Math.floor(value);
    return Math.max(MIN_ZOOM, z);
}

function createNaTileLayer(tilesUrl, maxNativeZoom, maxZoom) {
    const L = getLeaflet();

    const naBoundsLatLng = L.latLngBounds(
        [NA_BOUNDS.latMin, NA_BOUNDS.lonMin],
        [NA_BOUNDS.latMax, NA_BOUNDS.lonMax]
    );

    const layer = L.tileLayer(tilesUrl, {
        bounds: naBoundsLatLng,
        minZoom: MIN_ZOOM,
        maxZoom: maxZoom,
        maxNativeZoom: maxNativeZoom,
        noWrap: true,
        attribution: "Local tiles",
    });

    try {
        console.log("[GS26 map] tile layer created", {
            tilesUrl,
            maxNativeZoom,
            maxZoom,
        });
        layer.on("loading", () => console.log("[GS26 map] tiles loading"));
        layer.on("tileloadstart", (e) => console.log("[GS26 map] tileloadstart", e?.url || ""));
        layer.on("tileload", (e) => console.log("[GS26 map] tileload", e?.tile?.src || e?.url || ""));
        layer.on("tileerror", (e) => console.warn("[GS26 map] tileerror", e?.tile?.src || e?.url || "", e));
    } catch (e) {
        console.warn("[GS26 map] failed to install tile logging", e);
    }

    return layer;
}

function rememberMapView() {
    if (!groundMap) return;
    const c = groundMap.getCenter();
    lastMapView = {lat: c.lat, lon: c.lng, zoom: groundMap.getZoom()};
}

// ============================================================================
// Marker creation
// ============================================================================

function makeEmojiIcon(char, extraClass) {
    const L = getLeaflet();
    ensureMarkerStylesOnce();

    return L.divIcon({
        html: `
      <div class="user-marker-wrapper">
        <span class="emoji-marker ${extraClass || ""}">${char}</span>
      </div>
    `,
        className: "",
        iconSize: [MARKER_PX, MARKER_PX],
        iconAnchor: [MARKER_ANCHOR, MARKER_ANCHOR],
    });
}

function makeUserIcon() {
    const L = getLeaflet();
    ensureMarkerStylesOnce();

    return L.divIcon({
        html: `
      <div class="user-marker-wrapper">
        <span class="emoji-marker user-base">🧍</span>
        <span class="emoji-marker user-heading-indicator">▲</span>
      </div>
    `,
        className: "",
        iconSize: [MARKER_PX, MARKER_PX],
        iconAnchor: [MARKER_ANCHOR, MARKER_ANCHOR],
    });
}

function updateUserMarkerRotation() {
    if (!userMarker || userHeadingDeg == null) return;

    const el = userMarker.getElement();
    if (!el) return;

    const arrow = el.querySelector(".user-heading-indicator");
    if (!arrow) return;

    arrow.style.transform =
        `translate(-50%, -50%) rotate(${userHeadingDeg}deg) translateY(-${ARROW_RADIUS}px)`;
}

function setGroundMapUserHeading(deg) {
    if (!Number.isFinite(deg)) return;
    nativeHeadingDeg = normalizeAngle(deg);
    applyFusedHeading();
}

function syncRocketGuideLine(rocketLatLng, userLatLng) {
    if (!groundMap) return;
    const L = getLeaflet();

    if (!rocketLatLng || !userLatLng) {
        if (rocketGuideLine) {
            try {
                groundMap.removeLayer(rocketGuideLine);
            } catch (e) {
            }
            rocketGuideLine = null;
        }
        return;
    }

    const points = [userLatLng, rocketLatLng];
    if (!rocketGuideLine) {
        rocketGuideLine = L.polyline(points, {
            color: "#ef4444",
            weight: 3,
            opacity: 0.95,
        }).addTo(groundMap);
        return;
    }

    rocketGuideLine.setLatLngs(points);
}

// ============================================================================
// Compass handling
// ============================================================================

function handleOrientation(event) {
    let heading = null;

    if (typeof event.webkitCompassHeading === "number") {
        heading = normalizeAngle(event.webkitCompassHeading);
    } else if (event.absolute === true && typeof event.alpha === "number") {
        heading = normalizeAngle(event.alpha);
    } else if (typeof event.alpha === "number") {
        heading = normalizeAngle(360 - event.alpha);
    }

    if (heading == null) return;
    deviceHeadingDeg = heading;
    applyFusedHeading();
}

function initCompassOnce() {
    if (compassInitialized) return;
    compassInitialized = true;
    if (window.__gs26_disable_compass === true) return;

    if (!window.DeviceOrientationEvent) return;

    const Dev = DeviceOrientationEvent;
    if (typeof Dev.requestPermission === "function") {
        const KEY = "gs26_compass_permission_v1";

        let saved;
        try {
            saved = window.localStorage ? (window.localStorage.getItem(KEY) || "") : "";
        } catch (e) {
            saved = "";
        }

        if (saved === "granted") {
            window.addEventListener("deviceorientation", handleOrientation);
            return;
        }
        if (saved === "denied") {
            return;
        }

        Dev.requestPermission()
            .then((s) => {
                try {
                    if (window.localStorage) window.localStorage.setItem(KEY, s || "denied");
                } catch (e) {
                }
                if (s === "granted") {
                    window.addEventListener("deviceorientation", handleOrientation);
                }
            })
            .catch(() => {
                try {
                    if (window.localStorage) window.localStorage.setItem(KEY, "denied");
                } catch (e) {
                }
            });
    } else {
        window.addEventListener("deviceorientation", handleOrientation);
    }
}

// ============================================================================
// wasm-bindgen exports
// ============================================================================

function centerGroundMapOn(lat, lon) {
    if (!groundMap) return;
    groundMap.setView([lat, lon], groundMap.getZoom());
}

function getLastUserLatLng() {
    if (!lastUserLatLng) return null;
    return {lat: lastUserLatLng[0], lon: lastUserLatLng[1]};
}

function trackedAssetTitle() {
    return window.__gs26_tracked_asset_title || "Tracked Asset";
}

function initGroundMap(tilesUrl, centerLat, centerLon, zoom, maxNativeZoom, assetTitle) {
    const L = getLeaflet();
    ensureMarkerStylesOnce();
    initCompassOnce();
    window.__gs26_tracked_asset_title = assetTitle || trackedAssetTitle();
    const effectiveMaxNativeZoom = clampMaxNativeZoom(maxNativeZoom);
    const effectiveMaxZoom = effectiveMaxNativeZoom + DEFAULT_MAX_OVERZOOM_DELTA;
    const desiredZoom = lastMapView ? lastMapView.zoom : zoom;
    const clampedZoom = Math.min(effectiveMaxZoom, Math.max(MIN_ZOOM, desiredZoom));

    const el = document.getElementById("ground-map");
    if (!el) return;

    try {
        console.log("[GS26 map] initGroundMap", {
            tilesUrl,
            centerLat,
            centerLon,
            zoom,
            maxNativeZoom,
        });
    } catch (e) {
    }

    if (groundMap && groundMap.getContainer() === el) {
        const configChanged =
            currentTilesUrl !== tilesUrl ||
            currentMaxNativeZoom !== effectiveMaxNativeZoom ||
            currentMaxZoom !== effectiveMaxZoom;

        if (configChanged) {
            if (groundTileLayer) {
                try {
                    groundMap.removeLayer(groundTileLayer);
                } catch (e) {
                }
            }

            groundMap.setMinZoom(MIN_ZOOM);
            groundMap.setMaxZoom(effectiveMaxZoom);
            groundTileLayer = createNaTileLayer(
                tilesUrl,
                effectiveMaxNativeZoom,
                effectiveMaxZoom
            );
            groundTileLayer.addTo(groundMap);
            currentTilesUrl = tilesUrl;
            currentMaxNativeZoom = effectiveMaxNativeZoom;
            currentMaxZoom = effectiveMaxZoom;

            const nextZoom = Math.min(
                effectiveMaxZoom,
                Math.max(MIN_ZOOM, groundMap.getZoom())
            );
            if (nextZoom !== groundMap.getZoom()) {
                groundMap.setZoom(nextZoom);
            }
        }

        try {
            groundMap.invalidateSize();
        } catch (e) {
        }
        return;
    }
    if (groundMap) {
        groundMap.remove();
        window.__gs26_ground_map = null;
        groundTileLayer = null;
        rocketGuideLine = null;
    }

    groundMap = L.map(el, {
        center: lastMapView ? [lastMapView.lat, lastMapView.lon] : [centerLat, centerLon],
        zoom: clampedZoom,
        minZoom: MIN_ZOOM,
        maxZoom: effectiveMaxZoom,
    });

    groundTileLayer = createNaTileLayer(tilesUrl, effectiveMaxNativeZoom, effectiveMaxZoom);
    groundTileLayer.addTo(groundMap);
    currentTilesUrl = tilesUrl;
    currentMaxNativeZoom = effectiveMaxNativeZoom;
    currentMaxZoom = effectiveMaxZoom;
    groundMap.on("moveend zoomend", rememberMapView);
    rememberMapView();
    window.__gs26_ground_map = groundMap;

    if (lastRocketLatLng) {
        rocketMarker = L.marker(lastRocketLatLng, {
            icon: makeEmojiIcon("🚀", "rocket-marker"),
            title: trackedAssetTitle(),
        }).addTo(groundMap);
    }

    if (lastUserLatLng) {
        userMarker = L.marker(lastUserLatLng, {
            icon: makeUserIcon(),
            title: "You",
        }).addTo(groundMap);
        updateUserMarkerRotation();
    }

    syncRocketGuideLine(lastRocketLatLng, lastUserLatLng);
}

function updateGroundMapMarkers(rLat, rLon, uLat, uLon) {
    const hasRocket = Number.isFinite(rLat) && Number.isFinite(rLon);
    const hasUser = Number.isFinite(uLat) && Number.isFinite(uLon);

    if (hasRocket) {
        lastRocketLatLng = [rLat, rLon];
    }

    if (hasUser) {
        lastUserLatLng = [uLat, uLon];
    }

    if (!groundMap) return;
    const L = getLeaflet();

    if (hasRocket) {
        if (!rocketMarker) {
            rocketMarker = L.marker(lastRocketLatLng, {
                icon: makeEmojiIcon("🚀", "rocket-marker"),
                title: trackedAssetTitle(),
            }).addTo(groundMap);
        } else {
            rocketMarker.setLatLng(lastRocketLatLng);
        }
    }

    if (hasUser) {
        if (!userMarker) {
            userMarker = L.marker(lastUserLatLng, {
                icon: makeUserIcon(),
                title: "You",
            }).addTo(groundMap);
            updateUserMarkerRotation();
        } else {
            userMarker.setLatLng(lastUserLatLng);
        }
    }

    syncRocketGuideLine(hasRocket ? lastRocketLatLng : null, hasUser ? lastUserLatLng : null);
}

// ---- keep as global script ----
(function pinGroundStation26() {
    // Put everything on ONE namespace so it’s easy to inspect/debug.
    const api = (window.GS26 = window.GS26 || {});

    // Public API you call from Rust:
    api.initGroundMap = initGroundMap;
    api.updateGroundMapMarkers = updateGroundMapMarkers;
    api.centerGroundMapOn = centerGroundMapOn;
    api.getLastUserLatLng = getLastUserLatLng;

    // Optional: expose these too (useful for debugging / permissions testing)
    api.initCompassOnce = initCompassOnce;
    api.handleOrientation = handleOrientation;

    // Pin “internal” helpers so minifiers don’t decide they’re dead:
    api.getLeaflet = getLeaflet;
    api.normalizeAngle = normalizeAngle;
    api.shortestAngleDiff = shortestAngleDiff;

    api.ensureMarkerStylesOnce = ensureMarkerStylesOnce;
    api.createNaTileLayer = createNaTileLayer;
    api.rememberMapView = rememberMapView;

    api.makeEmojiIcon = makeEmojiIcon;
    api.makeUserIcon = makeUserIcon;
    api.updateUserMarkerRotation = updateUserMarkerRotation;
    api.setGroundMapUserHeading = setGroundMapUserHeading;
    api.syncRocketGuideLine = syncRocketGuideLine;

    // Pin state too (lets you debug on-device):
    api.state = api.state || {};
    Object.assign(api.state, {
        NA_BOUNDS,
        MIN_ZOOM,
        DEFAULT_MAX_NATIVE_ZOOM,
        MARKER_PX,
        MARKER_ANCHOR,
        USER_FONT_PX,
        ROCKET_FONT_PX,
        ARROW_FONT_PX,
        ARROW_RADIUS,

        // live mutable state pointers (debug only)
        get groundMap() {
            return groundMap;
        },
        get rocketMarker() {
            return rocketMarker;
        },
        get userMarker() {
            return userMarker;
        },
        get lastRocketLatLng() {
            return lastRocketLatLng;
        },
        get lastUserLatLng() {
            return lastUserLatLng;
        },
        get lastMapView() {
            return lastMapView;
        },
        get userHeadingDegRaw() {
            return userHeadingDegRaw;
        },
        get userHeadingDeg() {
            return userHeadingDeg;
        },
        get compassInitialized() {
            return compassInitialized;
        },
    });

    window.initGroundMap = api.initGroundMap;
    window.updateGroundMapMarkers = api.updateGroundMapMarkers;
    window.centerGroundMapOn = api.centerGroundMapOn;
    window.getLastUserLatLng = api.getLastUserLatLng;
    window.initCompassOnce = api.initCompassOnce;
    window.setGroundMapUserHeading = api.setGroundMapUserHeading;

    // “Loaded” flag
    window.__gs26_ground_station_loaded = true;
    console.log("[GS26] ground_station.js loaded; keys:", Object.keys(api));
})();
