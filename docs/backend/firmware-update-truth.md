# Live firmware update results

The `prepared_reboot_unverified` terminal status means the board accepted FINISH and reported the patch prepared. A reset was requested; the ground station has not verified a reboot or the running firmware image. Launch may prevent the reset. A full progress bar measures transferred bytes, not boot success. Verify the board before a new explicit update; the client requires renewed acknowledgement and never retries or resets automatically.

No current telemetry binds a running firmware version/image hash to this update transaction. A different boot count, discovery response or stream close alone is insufficient: it could represent a power cycle, rollback or another image. The UI also labels legacy `completed` and `rebooting` statuses as reboot-unverified.

Client firmware requests retain their original station, credential revision, connection epoch, selected target and tab lifetime. Responses from a retired owner do not repaint status, clear a new account's session, or issue a cancel against the new station. Cancellation cannot recall bytes already accepted by a socket/backend or stop an already-admitted server transfer.

This result/ownership change does not add the missing backend Idle/Recovery and per-operation original-origin controls. Backend authorization still checks SendCommands and FirmwareUpdate at admission; asynchronous state/permission/retention followthrough requires its separate safety change and qualification. Do not interpret the result phase as proof that active-flight OTA is safe or that failed/cancelled transfers rolled back remotely.
