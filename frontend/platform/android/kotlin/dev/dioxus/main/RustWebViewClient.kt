package dev.dioxus.main

import android.content.Context
import android.graphics.Bitmap
import android.net.Uri
import android.os.Handler
import android.os.Looper
import android.util.Log
import android.webkit.WebResourceError
import android.webkit.WebResourceRequest
import android.webkit.WebResourceResponse
import android.webkit.WebView
import android.webkit.WebViewClient
import androidx.webkit.WebViewAssetLoader
import org.json.JSONObject
import java.io.ByteArrayInputStream
import java.io.File
import java.io.InputStream
import java.net.HttpURLConnection
import java.net.URL
import java.security.SecureRandom
import java.security.cert.X509Certificate
import javax.net.ssl.HostnameVerifier
import javax.net.ssl.HttpsURLConnection
import javax.net.ssl.SSLContext
import javax.net.ssl.TrustManager
import javax.net.ssl.X509TrustManager

class RustWebViewClient(private val context: Context): WebViewClient() {
    private val tag = "GS26WebView"
    private val interceptedState = mutableMapOf<String, Boolean>()
    var currentUrl: String = "about:blank"
    private var lastInterceptedUrl: Uri? = null
    private var pendingUrlRedirect: String? = null

    private val assetLoader = WebViewAssetLoader.Builder()
        .setDomain(assetLoaderDomain())
        .addPathHandler("/", WebViewAssetLoader.AssetsPathHandler(context))
        .build()

    override fun shouldInterceptRequest(
        view: WebView,
        request: WebResourceRequest
    ): WebResourceResponse? {
        Log.e(tag, "shouldInterceptRequest url=${request.url} method=${request.method} mainFrame=${request.isForMainFrame}")

        pendingUrlRedirect?.let {
            Handler(Looper.getMainLooper()).post {
                view.loadUrl(it)
            }
            pendingUrlRedirect = null
            return null
        }

        proxyTileRequest(request.url)?.let {
            Log.e(tag, "tile proxy handled url=${request.url}")
            interceptedState[request.url.toString()] = true
            return it
        }

        lastInterceptedUrl = request.url
        return if (withAssetLoader()) {
            assetLoader.shouldInterceptRequest(request.url)
        } else {
            val rustWebview = view as RustWebView
            val normalizedRequest = normalizeDioxusInternalRequest(request)
            val effectiveUrl = normalizedRequest.url.toString()
            val response = handleRequest(rustWebview.id, normalizedRequest, rustWebview.isDocumentStartScriptEnabled)
            interceptedState[effectiveUrl] = response != null
            if (effectiveUrl != request.url.toString()) {
                interceptedState[request.url.toString()] = response != null
            }
            Log.e(tag, "intercept result url=${request.url} effectiveUrl=$effectiveUrl handled=${response != null}")
            response
        }
    }

    override fun shouldOverrideUrlLoading(
        view: WebView,
        request: WebResourceRequest
    ): Boolean {
        return shouldOverride(request.url.toString())
    }

    override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {
        Log.e(tag, "onPageStarted url=$url")
        currentUrl = url
        if (interceptedState[url] == false) {
            val webView = view as RustWebView
            for (script in webView.initScripts) {
                view.evaluateJavascript(script, null)
            }
        }
        onPageLoading(url)
    }

    override fun onPageFinished(view: WebView, url: String) {
        Log.e(tag, "onPageFinished url=$url")
        onPageLoaded(url)
    }

    override fun onReceivedError(
        view: WebView,
        request: WebResourceRequest,
        error: WebResourceError
    ) {
        Log.e(tag, "onReceivedError url=${request.url} code=${error.errorCode} desc=${error.description}")
        if (error.errorCode == ERROR_CONNECT && request.isForMainFrame && request.url != lastInterceptedUrl) {
            view.stopLoading()
            view.loadUrl(request.url.toString())
            pendingUrlRedirect = request.url.toString()
        } else {
            super.onReceivedError(view, request, error)
        }
    }

    override fun onReceivedHttpError(
        view: WebView,
        request: WebResourceRequest,
        errorResponse: WebResourceResponse
    ) {
        Log.e(
            tag,
            "onReceivedHttpError url=${request.url} status=${errorResponse.statusCode} reason=${errorResponse.reasonPhrase}"
        )
        super.onReceivedHttpError(view, request, errorResponse)
    }

    private fun proxyTileRequest(uri: Uri): WebResourceResponse? {
        val host = uri.host ?: return null
        if (host != "gs26.local") {
            return null
        }
        val path = uri.path ?: return null
        if (!path.startsWith("/tiles/")) {
            return null
        }

        val baseUrl = loadStoredBaseUrl() ?: return errorResponse(503, "Service Unavailable", "Missing backend URL")
        val normalizedBase = normalizeBaseUrl(baseUrl)
        if (normalizedBase.isEmpty()) {
            return errorResponse(503, "Service Unavailable", "Invalid backend URL")
        }

        val upstreamUrl = normalizedBase.trimEnd('/') + path
        val skipTlsVerify = loadSkipTlsVerify(normalizedBase)
        Log.e(tag, "proxying tile uri=$uri upstream=$upstreamUrl skipTls=$skipTlsVerify")

        return try {
            val connection = openConnection(upstreamUrl, skipTlsVerify)
            connection.instanceFollowRedirects = true
            connection.connectTimeout = 5000
            connection.readTimeout = 10000
            connection.setRequestProperty("User-Agent", "GS26-Android-WebView")
            connection.connect()

            val status = connection.responseCode
            val reason = connection.responseMessage ?: "OK"
            val mimeType = connection.contentType?.substringBefore(';') ?: "image/jpeg"
            val encoding = connection.contentEncoding ?: "binary"
            val input = selectResponseStream(connection)
            val headers = mutableMapOf<String, String>()
            connection.headerFields.forEach { (key, values) ->
                if (key != null && values != null && values.isNotEmpty()) {
                    headers[key] = values.joinToString(",")
                }
            }

            WebResourceResponse(mimeType, encoding, status, reason, headers, input)
        } catch (exc: Exception) {
            Log.e(tag, "tile proxy failed uri=$uri error=${exc.message}", exc)
            errorResponse(502, "Bad Gateway", exc.message ?: "Tile proxy failed")
        }
    }

    private fun openConnection(url: String, skipTlsVerify: Boolean): HttpURLConnection {
        val connection = URL(url).openConnection() as HttpURLConnection
        if (skipTlsVerify && connection is HttpsURLConnection) {
            val trustAll = arrayOf<TrustManager>(object : X509TrustManager {
                override fun checkClientTrusted(chain: Array<X509Certificate>, authType: String) {}
                override fun checkServerTrusted(chain: Array<X509Certificate>, authType: String) {}
                override fun getAcceptedIssuers(): Array<X509Certificate> = emptyArray()
            })
            val sslContext = SSLContext.getInstance("TLS")
            sslContext.init(null, trustAll, SecureRandom())
            connection.sslSocketFactory = sslContext.socketFactory
            connection.hostnameVerifier = HostnameVerifier { _, _ -> true }
        }
        return connection
    }

    private fun selectResponseStream(connection: HttpURLConnection): InputStream {
        return try {
            connection.inputStream
        } catch (_: Exception) {
            connection.errorStream ?: ByteArrayInputStream(ByteArray(0))
        }
    }

    private fun errorResponse(status: Int, reason: String, message: String): WebResourceResponse {
        return WebResourceResponse(
            "text/plain",
            "utf-8",
            status,
            reason,
            emptyMap(),
            ByteArrayInputStream(message.toByteArray())
        )
    }

    private fun loadStoredBaseUrl(): String? {
        val storageFile = File(context.filesDir, "gs26/storage.json")
        if (!storageFile.exists()) {
            return null
        }
        return try {
            JSONObject(storageFile.readText()).optString("gs_base_url").takeIf { it.isNotBlank() }
        } catch (exc: Exception) {
            Log.e(tag, "failed to read storage.json: ${exc.message}", exc)
            null
        }
    }

    private fun loadSkipTlsVerify(baseUrl: String): Boolean {
        val storageFile = File(context.filesDir, "gs26/storage.json")
        if (!storageFile.exists()) {
            return false
        }
        return try {
            val key = "gs_skip_tls_verify_${normalizeBaseUrl(baseUrl)}"
            JSONObject(storageFile.readText()).optString(key) == "true"
        } catch (_: Exception) {
            false
        }
    }

    private fun normalizeBaseUrl(value: String): String {
        var base = value.trim()
        val hashIndex = base.indexOf('#')
        if (hashIndex >= 0) {
            base = base.substring(0, hashIndex)
        }
        val schemeIndex = base.indexOf("://")
        if (schemeIndex >= 0) {
            val rest = base.substring(schemeIndex + 3)
            val slashIndex = rest.indexOf('/')
            if (slashIndex >= 0) {
                base = base.substring(0, schemeIndex + 3 + slashIndex)
            }
        }
        return base.trimEnd('/')
    }

    private fun normalizeDioxusInternalRequest(request: WebResourceRequest): WebResourceRequest {
        val url = request.url
        val host = url.host ?: return request
        val encodedPath = url.encodedPath ?: return request

        // Dioxus internal event endpoint sometimes appears as `//__events` on Android.
        // Normalize to `/__events` before handing off to native request routing.
        if (host != "dioxus.index.html" || !encodedPath.startsWith("//")) {
            return request
        }

        val normalized = url.buildUpon().encodedPath(encodedPath.replaceFirst("//", "/")).build()
        if (normalized == url) {
            return request
        }

        Log.e(tag, "normalized dioxus request url=$url -> $normalized")
        return object : WebResourceRequest {
            override fun getUrl(): Uri = normalized
            override fun isForMainFrame(): Boolean = request.isForMainFrame
            override fun isRedirect(): Boolean = request.isRedirect()
            override fun hasGesture(): Boolean = request.hasGesture()
            override fun getMethod(): String = request.method
            override fun getRequestHeaders(): MutableMap<String, String> = request.requestHeaders
        }
    }

    companion object {
        init {
            System.loadLibrary("main")
        }
    }

    private external fun assetLoaderDomain(): String
    private external fun withAssetLoader(): Boolean
    private external fun handleRequest(webviewId: String, request: WebResourceRequest, isDocumentStartScriptEnabled: Boolean): WebResourceResponse?
    private external fun shouldOverride(url: String): Boolean
    private external fun onPageLoading(url: String)
    private external fun onPageLoaded(url: String)
}
