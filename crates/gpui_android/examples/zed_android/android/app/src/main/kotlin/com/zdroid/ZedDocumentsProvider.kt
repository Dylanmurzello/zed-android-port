package com.zdroid

import android.content.res.AssetFileDescriptor
import android.database.Cursor
import android.database.MatrixCursor
import android.graphics.Point
import android.os.CancellationSignal
import android.os.ParcelFileDescriptor
import android.provider.DocumentsContract
import android.provider.DocumentsProvider
import android.util.Log
import android.webkit.MimeTypeMap
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.util.LinkedList
import kotlin.io.path.ExperimentalPathApi
import kotlin.io.path.deleteRecursively

/// Exposes Zed's `~` (i.e. `/data/data/com.zdroid/files/home`) to
/// other apps via Android's Storage Access Framework. After install Zed
/// shows up in any system "Open from / Save to" picker as its own root —
/// same pattern as Termux. Read-write; users can browse, edit, share files
/// from `~` without ADB or run-as.
///
/// Lifecycle gotcha: ContentProviders attach earlier than Activities. The
/// system can fork our process, run `Application.onCreate`, then
/// `ContentProvider.onCreate`, then service queries — without ever
/// touching `MainActivity` or `android_main`. So the bootstrap extractor
/// and env setup haven't run yet on a cold provider query. We rely on
/// `ZedApplication.onCreate` mkdir-ing `~` so the root is at least
/// browsable; the provider also defends with an `isDirectory()` check.
///
/// Document IDs are absolute filesystem paths (matches Termux). Other apps
/// pass them back through URI grants, so every ID is checked to stay under
/// `~`. Symlink escape during search is blocked by canonical-path check.
class ZedDocumentsProvider : DocumentsProvider() {
    private lateinit var baseDir: File

    override fun onCreate(): Boolean {
        baseDir = File(context!!.filesDir, "home")
        return true
    }

    override fun queryRoots(projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_ROOT_PROJECTION)
        if (!baseDir.isDirectory) {
            // Defense-in-depth — `ZedApplication.onCreate` should have
            // mkdir'd this. If it didn't, return empty rather than crashing
            // the system DocumentsUI.
            Log.w(TAG, "queryRoots: $baseDir is not a directory; returning empty cursor")
            return cursor
        }
        cursor.newRow().apply {
            add(DocumentsContract.Root.COLUMN_ROOT_ID, baseDir.absolutePath)
            add(DocumentsContract.Root.COLUMN_DOCUMENT_ID, baseDir.absolutePath)
            add(DocumentsContract.Root.COLUMN_TITLE, "Zed")
            add(DocumentsContract.Root.COLUMN_SUMMARY, "Zed home directory")
            add(DocumentsContract.Root.COLUMN_MIME_TYPES, "*/*")
            add(
                DocumentsContract.Root.COLUMN_FLAGS,
                DocumentsContract.Root.FLAG_SUPPORTS_CREATE
                    or DocumentsContract.Root.FLAG_SUPPORTS_SEARCH
                    or DocumentsContract.Root.FLAG_SUPPORTS_IS_CHILD,
            )
            add(DocumentsContract.Root.COLUMN_AVAILABLE_BYTES, baseDir.usableSpace)
        }
        return cursor
    }

    override fun queryDocument(documentId: String, projection: Array<out String>?): Cursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        includeFile(cursor, getFileForDocId(documentId))
        return cursor
    }

    override fun queryChildDocuments(
        parentDocumentId: String,
        projection: Array<out String>?,
        sortOrder: String?,
    ): Cursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        val parent = getFileForDocId(parentDocumentId)
        try {
            parent.listFiles()?.forEach { includeFile(cursor, it) }
        } catch (t: Throwable) {
            Log.w(TAG, "queryChildDocuments($parentDocumentId): ${t.message}")
        }
        return cursor
    }

    override fun querySearchDocuments(
        rootId: String,
        query: String,
        projection: Array<out String>?,
    ): Cursor {
        val cursor = MatrixCursor(projection ?: DEFAULT_DOCUMENT_PROJECTION)
        val root = getFileForDocId(rootId)
        val rootCanonical = try {
            root.canonicalPath
        } catch (t: Throwable) {
            Log.w(TAG, "querySearchDocuments: cannot resolve canonical path for root: ${t.message}")
            return cursor
        }
        val needle = query.lowercase()
        val pending = LinkedList<File>()
        pending.add(root)
        while (pending.isNotEmpty() && cursor.count < SEARCH_MAX) {
            val file = pending.removeFirst()
            // Symlink-escape protection: drop anything whose canonical
            // path leaves the root subtree (e.g. ~/storage/shared symlink
            // would otherwise expose all of /sdcard during search).
            try {
                if (!file.canonicalPath.startsWith(rootCanonical)) continue
            } catch (t: Throwable) {
                continue
            }
            if (file.isDirectory) {
                // Skip the dev-tree black holes that match nothing useful
                // and explode the walk. User can still navigate into them
                // manually for queryChildDocuments — this only filters the
                // recursive search.
                if (file != root && file.name in SKIP_DIRS) continue
                try {
                    file.listFiles()?.forEach { pending.add(it) }
                } catch (t: Throwable) {
                    continue
                }
            } else if (file.name.lowercase().contains(needle)) {
                includeFile(cursor, file)
            }
        }
        return cursor
    }

    override fun openDocument(
        documentId: String,
        mode: String,
        signal: CancellationSignal?,
    ): ParcelFileDescriptor {
        val file = getFileForDocId(documentId)
        return ParcelFileDescriptor.open(file, ParcelFileDescriptor.parseMode(mode))
    }

    override fun openDocumentThumbnail(
        documentId: String,
        sizeHint: Point,
        signal: CancellationSignal?,
    ): AssetFileDescriptor {
        val file = getFileForDocId(documentId)
        val pfd = ParcelFileDescriptor.open(file, ParcelFileDescriptor.MODE_READ_ONLY)
        return AssetFileDescriptor(pfd, 0, AssetFileDescriptor.UNKNOWN_LENGTH)
    }

    override fun createDocument(
        parentDocumentId: String,
        mimeType: String,
        displayName: String,
    ): String {
        val parent = getFileForDocId(parentDocumentId)
        var target = childFile(parent, displayName)
        var counter = 2
        while (target.exists()) {
            // Termux behavior: append " (N)" before extension on conflict.
            val (base, ext) = splitNameAndExt(displayName)
            target = File(parent, "$base ($counter)$ext")
            counter++
        }
        try {
            val ok = if (mimeType == DocumentsContract.Document.MIME_TYPE_DIR) {
                target.mkdir()
            } else {
                target.createNewFile()
            }
            if (!ok) throw FileNotFoundException("createDocument failed: ${target.absolutePath}")
        } catch (t: Throwable) {
            throw FileNotFoundException("createDocument: ${t.message}")
        }
        return target.absolutePath
    }

    // Deletes symlinks, not what they point to: `~/storage/shared` links to
    // shared storage. Kotlin's `File.deleteRecursively` follows them.
    @OptIn(ExperimentalPathApi::class)
    override fun deleteDocument(documentId: String) {
        val file = getFileForDocId(documentId)
        try {
            file.toPath().deleteRecursively()
        } catch (e: IOException) {
            throw FileNotFoundException("deleteDocument failed: $documentId: ${e.message}")
        }
    }

    override fun renameDocument(documentId: String, displayName: String): String {
        val file = getFileForDocId(documentId)
        val parent = file.parentFile ?: throw FileNotFoundException("rename: no parent for $documentId")
        var target = childFile(parent, displayName)
        var counter = 2
        while (target.exists() && target != file) {
            val (base, ext) = splitNameAndExt(displayName)
            target = File(parent, "$base ($counter)$ext")
            counter++
        }
        if (!file.renameTo(target)) {
            throw FileNotFoundException("renameDocument failed: $documentId → ${target.absolutePath}")
        }
        return target.absolutePath
    }

    // Compares whole path components after resolving `..`, so neither
    // `/foo` vs `/foobar` nor `/foo/../bar` passes.
    override fun isChildDocument(parentDocumentId: String, documentId: String): Boolean =
        File(documentId).normalize().startsWith(File(parentDocumentId).normalize())

    override fun getDocumentType(documentId: String): String =
        getMime(getFileForDocId(documentId))

    private fun includeFile(cursor: MatrixCursor, file: File) {
        val mime = getMime(file)
        var flags = 0
        val parentWritable = file.parentFile?.canWrite() == true
        if (file.isDirectory) {
            if (file.canWrite()) {
                flags = flags or DocumentsContract.Document.FLAG_DIR_SUPPORTS_CREATE
            }
        } else if (file.canWrite()) {
            flags = flags or DocumentsContract.Document.FLAG_SUPPORTS_WRITE
        }
        if (parentWritable) {
            flags = flags or DocumentsContract.Document.FLAG_SUPPORTS_DELETE
            flags = flags or DocumentsContract.Document.FLAG_SUPPORTS_RENAME
        }
        if (mime.startsWith("image/")) {
            flags = flags or DocumentsContract.Document.FLAG_SUPPORTS_THUMBNAIL
        }
        cursor.newRow().apply {
            add(DocumentsContract.Document.COLUMN_DOCUMENT_ID, file.absolutePath)
            add(DocumentsContract.Document.COLUMN_DISPLAY_NAME, file.name)
            add(DocumentsContract.Document.COLUMN_SIZE, file.length())
            add(DocumentsContract.Document.COLUMN_MIME_TYPE, mime)
            add(DocumentsContract.Document.COLUMN_LAST_MODIFIED, file.lastModified())
            add(DocumentsContract.Document.COLUMN_FLAGS, flags)
        }
    }

    // Resolves `..` without following symlinks, so `~/storage/shared` stays
    // browsable while `~/../databases` is refused.
    private fun getFileForDocId(docId: String): File {
        val file = File(docId).normalize()
        if (!file.isAbsolute || !file.startsWith(baseDir)) {
            throw FileNotFoundException("$docId is outside $baseDir")
        }
        if (!file.exists()) throw FileNotFoundException("$docId not found")
        return file
    }

    // Display names come from other apps; a `/` or `..` would create or
    // rename files outside `parent`.
    private fun childFile(parent: File, displayName: String): File {
        if (displayName.isEmpty() || displayName == "." || displayName == ".." ||
            displayName.contains(File.separatorChar)
        ) {
            throw FileNotFoundException("invalid document name: $displayName")
        }
        return File(parent, displayName)
    }

    private fun getMime(file: File): String {
        if (file.isDirectory) return DocumentsContract.Document.MIME_TYPE_DIR
        val ext = file.extension.lowercase()
        return CUSTOM_MIME[ext]
            ?: MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
            ?: "application/octet-stream"
    }

    private fun splitNameAndExt(name: String): Pair<String, String> {
        val dot = name.lastIndexOf('.')
        return if (dot <= 0) Pair(name, "") else Pair(name.substring(0, dot), name.substring(dot))
    }

    companion object {
        private const val TAG = "ZedDocsProvider"
        private const val SEARCH_MAX = 50

        private val DEFAULT_ROOT_PROJECTION = arrayOf(
            DocumentsContract.Root.COLUMN_ROOT_ID,
            DocumentsContract.Root.COLUMN_MIME_TYPES,
            DocumentsContract.Root.COLUMN_FLAGS,
            DocumentsContract.Root.COLUMN_TITLE,
            DocumentsContract.Root.COLUMN_SUMMARY,
            DocumentsContract.Root.COLUMN_DOCUMENT_ID,
            DocumentsContract.Root.COLUMN_AVAILABLE_BYTES,
        )

        private val DEFAULT_DOCUMENT_PROJECTION = arrayOf(
            DocumentsContract.Document.COLUMN_DOCUMENT_ID,
            DocumentsContract.Document.COLUMN_MIME_TYPE,
            DocumentsContract.Document.COLUMN_DISPLAY_NAME,
            DocumentsContract.Document.COLUMN_LAST_MODIFIED,
            DocumentsContract.Document.COLUMN_FLAGS,
            DocumentsContract.Document.COLUMN_SIZE,
        )

        // `MimeTypeMap.getSingleton()` doesn't know about most dev-language
        // extensions and falls back to `application/octet-stream` — files
        // with that MIME often don't show up in receiving apps that filter
        // on `text/*` (share sheet, "open with" pickers). Provide explicit
        // text/x-* mappings so .rs / .toml / .md / etc. surface as text.
        private val CUSTOM_MIME = mapOf(
            "rs" to "text/rust",
            "toml" to "application/toml",
            "md" to "text/markdown",
            "go" to "text/x-go",
            "py" to "text/x-python",
            "ts" to "text/typescript",
            "tsx" to "text/typescript",
            "kt" to "text/x-kotlin",
            "swift" to "text/x-swift",
            "zig" to "text/x-zig",
            "lock" to "text/plain",
            "yaml" to "application/yaml",
            "yml" to "application/yaml",
            "env" to "text/plain",
            "gitignore" to "text/plain",
            "dockerignore" to "text/plain",
            "dockerfile" to "text/plain",
        )

        // Skip during recursive search — these dirs commonly contain tens
        // of thousands of files that match no useful query and burn the
        // 50-result cap before reaching real source. Skipped only during
        // search-walk; users can still navigate into them via the picker.
        private val SKIP_DIRS = setOf(
            ".git",
            "node_modules",
            "target",
            "__pycache__",
            ".venv",
            "venv",
            ".cargo",
            ".rustup",
            ".npm",
            ".yarn",
            "build",
            "dist",
            ".next",
            ".gradle",
            ".idea",
        )
    }
}
