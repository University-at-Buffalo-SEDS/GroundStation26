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
import android.view.Surface
import android.view.WindowManager

object LocationShim : LocationListener, SensorEventListener {
    private const val LOCATION_PERMISSION_REQUEST_CODE = 2601
    private const val LOCATION_MIN_TIME_MS = 250L
    private const val LOCATION_MIN_DISTANCE_M = 0.25f

    private val mainHandler = Handler(Looper.getMainLooper())

    private var locationManager: LocationManager? = null
    private var sensorManager: SensorManager? = null
    private var rotationVectorSensor: Sensor? = null
    private var activity: Activity? = null
    private var started = false
    private var permissionRequested = false

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
            }
        } else {
            locationManager?.removeUpdates(this)
            sensorManager?.unregisterListener(this)
            started = false
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
        val adjustedRotationMatrix = FloatArray(9)
        val orientation = FloatArray(3)
        SensorManager.getRotationMatrixFromVector(rotationMatrix, event.values)
        val displayRotation = activity?.display?.rotation ?: Surface.ROTATION_0
        when (displayRotation) {
            Surface.ROTATION_90 -> SensorManager.remapCoordinateSystem(
                rotationMatrix,
                SensorManager.AXIS_Y,
                SensorManager.AXIS_MINUS_X,
                adjustedRotationMatrix
            )
            Surface.ROTATION_180 -> SensorManager.remapCoordinateSystem(
                rotationMatrix,
                SensorManager.AXIS_MINUS_X,
                SensorManager.AXIS_MINUS_Y,
                adjustedRotationMatrix
            )
            Surface.ROTATION_270 -> SensorManager.remapCoordinateSystem(
                rotationMatrix,
                SensorManager.AXIS_MINUS_Y,
                SensorManager.AXIS_X,
                adjustedRotationMatrix
            )
            else -> rotationMatrix.copyInto(adjustedRotationMatrix)
        }
        SensorManager.getOrientation(adjustedRotationMatrix, orientation)
        var headingDeg = Math.toDegrees(orientation[0].toDouble()).toFloat()
        if (headingDeg < 0f) {
            headingDeg += 360f
        }
        nativeOnHeadingUpdate(headingDeg)
    }

    override fun onAccuracyChanged(sensor: Sensor?, accuracy: Int) {}
}
