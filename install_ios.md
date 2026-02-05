# iOS code signing and provisioning (weekly)

## Overview

For development builds, Xcode generates a development signing certificate and a provisioning profile that must be
installed on the device. The provisioning profile expires after **7 days**, so the profile must be reinstalled weekly.

## Generate the iOS project (must match GroundStation)

When creating the iOS project in Xcode, **use the same name and identifier as GroundStation** so the provisioning
profile and bundle match the app.

Set these fields:

- **Product Name:** `GroundStation 26`
- **Bundle Identifier:** `com.UBSEDS.GS26`
- **Display Name:** `GroundStation 26`

If any of these differ, the generated provisioning profile will not install or the app will be treated as a different
bundle.

## Setup: codesign certificate in Xcode

1. Open **Xcode** and ensure you are signed in with your Apple ID:
    - **Xcode → Settings → Accounts** (or **Preferences → Accounts** on older versions).
    - Add your Apple ID and select the team you will use.
2. In the **Team** section, Xcode will create or download a **development certificate**.
3. Verify the certificate is available:
    - **Xcode → Settings → Accounts → Manage Certificates…**
    - You should see an **Apple Development** certificate for your team.

## Get `embedded.mobileconfig` from Xcode

1. Build or run the app on a connected device so Xcode generates a provisioning profile.
2. In Finder, open the build output for the app:
    - Xcode **Product → Show Build Folder in Finder**.
3. Locate the built `.app` bundle (e.g., `GroundStation.app`).
4. Inside the bundle, find `embedded.mobileprovision`.
5. Export it to a readable config:
    - **Xcode → Device and Simulators** window → select the device → **Installed Apps** → download the provisioning
      profile if needed.
    - Alternatively, convert the `.mobileprovision` to a `.mobileconfig` via the `profiles` tool or by opening the
      profile in Xcode and exporting it.
6. Save the resulting file as `embedded.mobileconfig`.
7. Copy the provisioning profile into the repo:
    - Place the file at `frontend/static/embedded.mobileprovision`.
    - This is bundled with the app build; it is **not** flashed directly to the device.

## Install on the device

1. AirDrop or otherwise copy `embedded.mobileconfig` to the iOS device.
2. Open the file on the device and install it via **Settings → General → VPN & Device Management**.
3. Trust the profile if prompted.

## Expiration (weekly)

- The development provisioning profile lasts **7 days**.
- After **1 week**, the app will stop launching until the profile is **reinstalled**.
- Repeat the steps above weekly to refresh the profile on the device.
