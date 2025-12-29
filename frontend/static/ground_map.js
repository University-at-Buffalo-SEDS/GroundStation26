//
// frontend/assets/ground_map.js
//
// Leaflet helpers for GroundStation26
// - ES module with named exports (required by wasm-bindgen)
// - Emoji-based markers (🚀 + 🧍) with heading indicator (▲)
// - Browser compass + geolocation support
//

let groundMap = null;
let rocketMarker = null;
let userMarker = null;

// Remember last-known positions across tab switches
let lastRocketLatLng = null;
let lastUserLatLng = null;
let lastMapView = null;

// you currently have tiles for z = 0..8
const MIN_ZOOM = 0;
const MAX_NATIVE_ZOOM = 12;

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

function createNaTileLayer(tilesUrl) {
  const L = getLeaflet();

  const naBoundsLatLng = L.latLngBounds(
    [NA_BOUNDS.latMin, NA_BOUNDS.lonMin],
    [NA_BOUNDS.latMax, NA_BOUNDS.lonMax]
  );

  return L.tileLayer(tilesUrl, {
    bounds: naBoundsLatLng,
    minZoom: MIN_ZOOM,
    maxZoom: MAX_NATIVE_ZOOM,
    maxNativeZoom: MAX_NATIVE_ZOOM,
    noWrap: true,
    attribution: "Local tiles",
  });
}

function rememberMapView() {
  if (!groundMap) return;
  const c = groundMap.getCenter();
  lastMapView = { lat: c.lat, lon: c.lng, zoom: groundMap.getZoom() };
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

  userHeadingDegRaw = heading;

  if (userHeadingDeg == null) {
    userHeadingDeg = heading;
  } else {
    const diff = shortestAngleDiff(userHeadingDeg, heading);
    if (Math.abs(diff) < 2) return;
    userHeadingDeg = normalizeAngle(userHeadingDeg + diff * 0.15);
  }

  updateUserMarkerRotation();
}

function initCompassOnce() {
  if (compassInitialized) return;
  compassInitialized = true;

  if (!window.DeviceOrientationEvent) return;

  const Dev = DeviceOrientationEvent;
  if (typeof Dev.requestPermission === "function") {
    Dev.requestPermission()
      .then((s) => s === "granted" && window.addEventListener("deviceorientation", handleOrientation))
      .catch(() => {});
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
  return { lat: lastUserLatLng[0], lon: lastUserLatLng[1] };
}

 function initGroundMap(tilesUrl, centerLat, centerLon, zoom) {
  const L = getLeaflet();
  ensureMarkerStylesOnce();
  initCompassOnce();

  const el = document.getElementById("ground-map");
  if (!el) return;

  if (groundMap && groundMap.getContainer() === el) return;
  if (groundMap) groundMap.remove();

  groundMap = L.map(el, {
    center: lastMapView ? [lastMapView.lat, lastMapView.lon] : [centerLat, centerLon],
    zoom: lastMapView ? lastMapView.zoom : zoom,
    minZoom: MIN_ZOOM,
    maxZoom: MAX_NATIVE_ZOOM,
  });

  createNaTileLayer(tilesUrl).addTo(groundMap);
  groundMap.on("moveend zoomend", rememberMapView);
  rememberMapView();

  if (lastRocketLatLng) {
    rocketMarker = L.marker(lastRocketLatLng, {
      icon: makeEmojiIcon("🚀", "rocket-marker"),
      title: "Rocket",
    }).addTo(groundMap);
  }

  if (lastUserLatLng) {
    userMarker = L.marker(lastUserLatLng, {
      icon: makeUserIcon(),
      title: "You",
    }).addTo(groundMap);
    updateUserMarkerRotation();
  }
}

 function updateGroundMapMarkers(rLat, rLon, uLat, uLon) {
  if (!groundMap) return;
  const L = getLeaflet();

  if (Number.isFinite(rLat) && Number.isFinite(rLon)) {
    lastRocketLatLng = [rLat, rLon];
    if (!rocketMarker) {
      rocketMarker = L.marker(lastRocketLatLng, {
        icon: makeEmojiIcon("🚀", "rocket-marker"),
        title: "Rocket",
      }).addTo(groundMap);
    } else {
      rocketMarker.setLatLng(lastRocketLatLng);
    }
  }

  if (Number.isFinite(uLat) && Number.isFinite(uLon)) {
    lastUserLatLng = [uLat, uLon];
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

  // Pin state too (lets you debug on-device):
  api.state = api.state || {};
  Object.assign(api.state, {
    NA_BOUNDS,
    MIN_ZOOM,
    MAX_NATIVE_ZOOM,
    MARKER_PX,
    MARKER_ANCHOR,
    USER_FONT_PX,
    ROCKET_FONT_PX,
    ARROW_FONT_PX,
    ARROW_RADIUS,

    // live mutable state pointers (debug only)
    get groundMap() { return groundMap; },
    get rocketMarker() { return rocketMarker; },
    get userMarker() { return userMarker; },
    get lastRocketLatLng() { return lastRocketLatLng; },
    get lastUserLatLng() { return lastUserLatLng; },
    get lastMapView() { return lastMapView; },
    get userHeadingDegRaw() { return userHeadingDegRaw; },
    get userHeadingDeg() { return userHeadingDeg; },
    get compassInitialized() { return compassInitialized; },
  });

  // Backwards-compat globals (so your Rust `window.*` calls still work)
  window.initGroundMap = api.initGroundMap;
  window.updateGroundMapMarkers = api.updateGroundMapMarkers;
  window.centerGroundMapOn = api.centerGroundMapOn;
  window.getLastUserLatLng = api.getLastUserLatLng;
  window.initCompassOnce = api.initCompassOnce;

  // “Loaded” flag
  window.__gs26_ground_station_loaded = true;
  console.log("[GS26] ground_station.js loaded; keys:", Object.keys(api));
})();