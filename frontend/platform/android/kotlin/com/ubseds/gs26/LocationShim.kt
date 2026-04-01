package com.ubseds.gs26

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.pm.PackageManager
import android.hardware.Sensor
import android.hardware.SensorEvent
import android.hardware.SensorEventListener
import android.hardware.SensorManager
import android.location.Location
import android.location.LocationListener
import android.location.LocationManager
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.view.WindowManager
import kotlin.math.abs
import kotlin.math.atan2
import kotlin.math.cos
import kotlin.math.sin

object LocationShim : LocationListener, SensorEventListener {
    private const val LOCATION_PERMISSION_REQUEST_CODE = 2601
    private const val LOCATION_MIN_TIME_MS = 250L
    private const val LOCATION_MIN_DISTANCE_M = 0.25f
    private const val HEADING_SMOOTHING_ALPHA_HIGH = 0.92f
    private const val HEADING_SMOOTHING_ALPHA_LOW = 0.78f
    private const val HEADING_JITTER_THRESHOLD_DEG = 0.12f
    private const val HEADING_SAMPLE_WINDOW = 1

    private val mainHandler = Handler(Looper.getMainLooper())

    private var locationManager: LocationManager? = null
    private var sensorManager: SensorManager? = null
    private var rotationVectorSensor: Sensor? = null
    private var activity: Activity? = null
    private var started = false
    private var permissionRequested = false
    private var smoothedHeadingDeg: Float? = null
    private var headingAccuracy: Int = SensorManager.SENSOR_STATUS_UNRELIABLE
    private val headingSamples = ArrayDeque<Float>()

    @JvmStatic
    external fun nativeOnLocationUpdate(lat: Double, lon: Double)

    @JvmStatic
    external fun nativeOnHeadingUpdate(headingDeg: Float)

    @JvmStatic
    fun start(context: Context) {
        val activity = context as? Activity ?: return
        this.activity = activity
        activity.runOnUiThread { ensureStarted(activity) }
    }

    @JvmStatic
    fun stop() {
        val activity = this.activity
        if (activity != null) {
            activity.runOnUiThread {
                locationManager?.removeUpdates(this)
                sensorManager?.unregisterListener(this)
                started = false
                smoothedHeadingDeg = null
                headingAccuracy = SensorManager.SENSOR_STATUS_UNRELIABLE
                headingSamples.clear()
            }
        } else {
            locationManager?.removeUpdates(this)
            sensorManager?.unregisterListener(this)
            started = false
            smoothedHeadingDeg = null
            headingAccuracy = SensorManager.SENSOR_STATUS_UNRELIABLE
            headingSamples.clear()
        }
    }

    @JvmStatic
    fun setKeepScreenOn(context: Context, enabled: Boolean) {
        val activity = context as? Activity ?: return
        activity.runOnUiThread {
            if (enabled) {
                activity.window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            } else {
                activity.window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            }
        }
    }

    private fun ensureStarted(activity: Activity) {
        if (!hasLocationPermission(activity)) {
            requestLocationPermission(activity)
            return
        }

        val appContext = activity.applicationContext
        if (locationManager == null) {
            locationManager = appContext.getSystemService(Context.LOCATION_SERVICE) as? LocationManager
        }
        if (sensorManager == null) {
            sensorManager = appContext.getSystemService(Context.SENSOR_SERVICE) as? SensorManager
            rotationVectorSensor = sensorManager?.getDefaultSensor(Sensor.TYPE_ROTATION_VECTOR)
        }

        val locationManager = locationManager ?: return
        val sensorManager = sensorManager ?: return

        if (started) {
            return
        }

        started = true

        try {
            locationManager.requestLocationUpdates(
                LocationManager.GPS_PROVIDER,
                LOCATION_MIN_TIME_MS,
                LOCATION_MIN_DISTANCE_M,
                this
            )
        } catch (_: SecurityException) {
            started = false
            return
        } catch (_: IllegalArgumentException) {
        }

        try {
            locationManager.requestLocationUpdates(
                LocationManager.NETWORK_PROVIDER,
                LOCATION_MIN_TIME_MS,
                LOCATION_MIN_DISTANCE_M,
                this
            )
        } catch (_: SecurityException) {
            started = false
            return
        } catch (_: IllegalArgumentException) {
        }

        rotationVectorSensor?.let {
            sensorManager.registerListener(this, it, SensorManager.SENSOR_DELAY_GAME)
        }

        tryEmitLastKnownLocation(locationManager)
    }

    private fun tryEmitLastKnownLocation(locationManager: LocationManager) {
        try {
            locationManager.getLastKnownLocation(LocationManager.GPS_PROVIDER)?.let {
                nativeOnLocationUpdate(it.latitude, it.longitude)
                return
            }
        } catch (_: SecurityException) {
        }

        try {
            locationManager.getLastKnownLocation(LocationManager.NETWORK_PROVIDER)?.let {
                nativeOnLocationUpdate(it.latitude, it.longitude)
            }
        } catch (_: SecurityException) {
        }
    }

    private fun hasLocationPermission(activity: Activity): Boolean {
        return activity.checkSelfPermission(Manifest.permission.ACCESS_FINE_LOCATION) == PackageManager.PERMISSION_GRANTED ||
            activity.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION) == PackageManager.PERMISSION_GRANTED
    }

    private fun requestLocationPermission(activity: Activity) {
        if (!permissionRequested) {
            permissionRequested = true
            activity.requestPermissions(
                arrayOf(
                    Manifest.permission.ACCESS_FINE_LOCATION,
                    Manifest.permission.ACCESS_COARSE_LOCATION
                ),
                LOCATION_PERMISSION_REQUEST_CODE
            )
        }

        mainHandler.postDelayed({
            if (hasLocationPermission(activity)) {
                permissionRequested = false
                ensureStarted(activity)
            }
        }, 1000L)
    }

    override fun onLocationChanged(location: Location) {
        nativeOnLocationUpdate(location.latitude, location.longitude)
    }

    override fun onProviderEnabled(provider: String) {}

    override fun onProviderDisabled(provider: String) {}

    override fun onStatusChanged(provider: String?, status: Int, extras: Bundle?) {}

    override fun onSensorChanged(event: SensorEvent) {
        if (event.sensor.type != Sensor.TYPE_ROTATION_VECTOR) {
            return
        }

        val rotationMatrix = FloatArray(9)
        SensorManager.getRotationMatrixFromVector(rotationMatrix, event.values)
        // Use absolute yaw from the raw world/device rotation matrix.
        // This keeps north stable regardless of screen rotation, pitch, or roll,
        // so only turning around the vertical axis changes the reported heading.
        var headingDeg = Math.toDegrees(atan2(rotationMatrix[1].toDouble(), rotationMatrix[4].toDouble())).toFloat()
        if (headingDeg < 0f) {
            headingDeg += 360f
        }
        if (headingAccuracy == SensorManager.SENSOR_STATUS_UNRELIABLE && smoothedHeadingDeg != null) {
            return
        }
        headingDeg = smoothHeading(headingDeg)
        nativeOnHeadingUpdate(headingDeg)
    }

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {
        if (sensor?.type == Sensor.TYPE_ROTATION_VECTOR) {
            headingAccuracy = accuracy
        }
    }

    private fun smoothHeading(sampleDeg: Float): Float {
        headingSamples.addLast(sampleDeg)
        while (headingSamples.size > HEADING_SAMPLE_WINDOW) {
            headingSamples.removeFirst()
        }
        val averagedSample = circularMeanDeg(headingSamples)
        val previous = smoothedHeadingDeg
        if (previous == null) {
            smoothedHeadingDeg = averagedSample
            return averagedSample
        }

        var delta = averagedSample - previous
        if (delta > 180f) {
            delta -= 360f
        } else if (delta < -180f) {
            delta += 360f
        }

        if (abs(delta) < HEADING_JITTER_THRESHOLD_DEG) {
            return previous
        }

        val alpha = if (headingAccuracy >= SensorManager.SENSOR_STATUS_ACCURACY_HIGH) {
            HEADING_SMOOTHING_ALPHA_HIGH
        } else {
            HEADING_SMOOTHING_ALPHA_LOW
        }
        val next = normalizeHeading(previous + (delta * alpha))
        smoothedHeadingDeg = next
        return next
    }

    private fun circularMeanDeg(values: Iterable<Float>): Float {
        var x = 0.0
        var y = 0.0
        var count = 0
        for (deg in values) {
            val rad = Math.toRadians(deg.toDouble())
            x += cos(rad)
            y += sin(rad)
            count += 1
        }
        if (count == 0) {
            return 0f
        }
        var deg = Math.toDegrees(atan2(y, x)).toFloat()
        if (deg < 0f) {
            deg += 360f
        }
        return deg
    }

    private fun normalizeHeading(value: Float): Float {
        var heading = value % 360f
        if (heading < 0f) {
            heading += 360f
        }
        if (abs(heading - 360f) < 0.001f) {
            heading = 0f
        }
        return heading
    }
}
