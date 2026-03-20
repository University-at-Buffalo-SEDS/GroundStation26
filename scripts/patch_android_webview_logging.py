#!/usr/bin/env python3

import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ANDROID_KOTLIN_DIR = (ROOT / "target" / "dx" / "groundstation_frontend" / "release" / "android" / "app" / "app" / 
                      "src" / "main" / "kotlin" / "dev" / "dioxus" / "main")
CLIENT_PATH = ANDROID_KOTLIN_DIR / "RustWebViewClient.kt"
WEBVIEW_PATH = ANDROID_KOTLIN_DIR / "RustWebView.kt"
CHROME_PATH = ANDROID_KOTLIN_DIR / "RustWebChromeClient.kt"


def patch_once(text: str, old: str, new: str, path: Path, *, required: bool = False) -> str:
    if new in text:
        return text
    if old not in text:
        if required:
            raise RuntimeError(f"expected snippet not found in {path}")
        return text
    return text.replace(old, new, 1)


def dedupe_import_line(text: str, import_line: str) -> str:
    lines = text.splitlines()
    seen = False
    out: list[str] = []
    for line in lines:
        if line == import_line:
            if seen:
                continue
            seen = True
        out.append(line)
    suffix = "\n" if text.endswith("\n") else ""
    return "\n".join(out) + suffix


def patch_client(text: str) -> str:
    text = patch_once(
        text,
        "package dev.dioxus.main\n\nimport android.net.Uri\n",
        "package dev.dioxus.main\n\nimport android.util.Log\nimport android.net.Uri\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "import android.net.Uri\nimport android.webkit.*\n",
        "import android.net.Uri\nimport android.util.Log\nimport android.webkit.*\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "class RustWebViewClient(context: Context): WebViewClient() {\n",
        'class RustWebViewClient(context: Context): WebViewClient() {\n    private val tag = "GS26WebView"\n',
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "    override fun shouldInterceptRequest(\n        view: WebView,\n        request: WebResourceRequest\n    "
        "): WebResourceResponse? {\n",
        "    override fun shouldInterceptRequest(\n        view: WebView,\n        request: WebResourceRequest\n    "
        "): WebResourceResponse? {\n        Log.e(tag, \"shouldInterceptRequest url=${request.url} method=${"
        "request.method} mainFrame=${request.isForMainFrame}\")\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "            val response = handleRequest(rustWebview.id, request, "
        "rustWebview.isDocumentStartScriptEnabled)\n            interceptedState[request.url.toString()] = response "
        "!= null\n            return response\n",
        "            val response = handleRequest(rustWebview.id, request, "
        "rustWebview.isDocumentStartScriptEnabled)\n            interceptedState[request.url.toString()] = response "
        "!= null\n            Log.e(tag, \"intercept result url=${request.url} handled=${response != null}\")\n       "
        "     return response\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "    override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {\n        currentUrl = url\n",
        "    override fun onPageStarted(view: WebView, url: String, favicon: Bitmap?) {\n        Log.e(tag, "
        "\"onPageStarted url=$url\")\n        currentUrl = url\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "    override fun onPageFinished(view: WebView, url: String) {\n        onPageLoaded(url)\n",
        "    override fun onPageFinished(view: WebView, url: String) {\n        Log.e(tag, \"onPageFinished "
        "url=$url\")\n        onPageLoaded(url)\n",
        CLIENT_PATH,
    )
    text = patch_once(
        text,
        "    override fun onReceivedError(\n        view: WebView,\n        request: WebResourceRequest,"
        "\n        error: WebResourceError\n    ) {\n",
        "    override fun onReceivedError(\n        view: WebView,\n        request: WebResourceRequest,"
        "\n        error: WebResourceError\n    ) {\n        Log.e(tag, \"onReceivedError url=${request.url} code=${"
        "error.errorCode} desc=${error.description}\")\n",
        CLIENT_PATH,
    )
    if "override fun onReceivedHttpError(" not in text:
        marker = "    companion object {\n"
        insert = (
            "    override fun onReceivedHttpError(\n"
            "        view: WebView,\n"
            "        request: WebResourceRequest,\n"
            "        errorResponse: WebResourceResponse\n"
            "    ) {\n"
            "        Log.e(\n"
            "            tag,\n"
            "            \"onReceivedHttpError url=${request.url} status=${errorResponse.statusCode} reason=${"
            "errorResponse.reasonPhrase}\"\n"
            "        )\n"
            "        super.onReceivedHttpError(view, request, errorResponse)\n"
            "    }\n\n"
        )
        if marker not in text:
            raise RuntimeError(f"expected insertion marker not found in {CLIENT_PATH}")
        text = text.replace(marker, insert + marker, 1)
    return dedupe_import_line(text, "import android.util.Log")


def patch_webview(text: str) -> str:
    text = patch_once(
        text,
        "import android.annotation.SuppressLint\nimport android.webkit.*\n",
        "import android.annotation.SuppressLint\nimport android.util.Log\nimport android.webkit.*\n",
        WEBVIEW_PATH,
    )
    text = patch_once(
        text,
        "import android.annotation.SuppressLint\nimport android.webkit.*\nimport android.content.Context\n",
        "import android.annotation.SuppressLint\nimport android.util.Log\nimport android.webkit.*\nimport "
        "android.content.Context\n",
        WEBVIEW_PATH,
    )
    text = patch_once(
        text,
        "class RustWebView(context: Context, val initScripts: Array<String>, val id: String): WebView(context) {\n",
        'class RustWebView(context: Context, val initScripts: Array<String>, val id: String): WebView(context) {\n    '
        'private val tag = "GS26WebView"\n',
        WEBVIEW_PATH,
    )
    text = patch_once(
        text,
        "    init {\n        settings.javaScriptEnabled = true\n",
        "    init {\n        Log.e(tag, \"RustWebView init id=$id\")\n        settings.javaScriptEnabled = true\n",
        WEBVIEW_PATH,
    )
    text = patch_once(
        text,
        "    override fun loadUrl(url: String) {\n        if (!shouldOverride(url)) {\n",
        "    override fun loadUrl(url: String) {\n        Log.e(tag, \"RustWebView loadUrl url=$url\")\n        if ("
        "!shouldOverride(url)) {\n",
        WEBVIEW_PATH,
    )
    text = patch_once(
        text,
        "    override fun loadUrl(url: String, additionalHttpHeaders: Map<String, String>) {\n        if ("
        "!shouldOverride(url)) {\n",
        "    override fun loadUrl(url: String, additionalHttpHeaders: Map<String, String>) {\n        Log.e(tag, "
        "\"RustWebView loadUrl with headers url=$url\")\n        if (!shouldOverride(url)) {\n",
        WEBVIEW_PATH,
    )
    return dedupe_import_line(text, "import android.util.Log")


def patch_chrome(text: str) -> str:
    text = patch_once(
        text,
        "import android.view.View\nimport android.webkit.*\n",
        "import android.util.Log\nimport android.view.View\nimport android.webkit.*\n",
        CHROME_PATH,
    )
    text = patch_once(
        text,
        "  override fun onConsoleMessage(consoleMessage: ConsoleMessage): Boolean {\n",
        "  override fun onConsoleMessage(consoleMessage: ConsoleMessage): Boolean {\n    Log.e(\"GS26WebView\", "
        "\"console ${consoleMessage.messageLevel()} ${consoleMessage.sourceId()}:${consoleMessage.lineNumber()} ${"
        "consoleMessage.message()}\")\n",
        CHROME_PATH,
    )
    return dedupe_import_line(text, "import android.util.Log")


def patch_file(path: Path, patcher) -> None:
    if not path.exists():
        raise RuntimeError(f"missing generated file: {path}")
    original = path.read_text()
    patched = patcher(original)
    if patched != original:
        path.write_text(patched)
        print(f"patched {path}")
    else:
        print(f"skipped {path}")


def main() -> int:
    try:
        patch_file(CLIENT_PATH, patch_client)
        patch_file(WEBVIEW_PATH, patch_webview)
        patch_file(CHROME_PATH, patch_chrome)
    except Exception as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
