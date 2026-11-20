package org.licash.wallet

import android.content.Context
import android.net.Uri
import android.provider.DocumentsContract
import android.provider.DocumentsContract.Document
import java.io.File
import java.io.IOException

/**
 * A folder of a tree the user chose (Storage Access Framework), as far as
 * backups need one: its files listed, written whole, read, deleted. Each
 * call reaches the folder's provider: off the main thread.
 */
internal class Folder private constructor(
    private val context: Context,
    private val tree: Uri,
    private val id: String,
) {
    /** A file or folder in it. */
    data class Child(val name: String, val bytes: Long, val id: String, val dir: Boolean)

    private val resolver get() = context.contentResolver
    private val uri: Uri get() = DocumentsContract.buildDocumentUriUsingTree(tree, id)

    /** Its own name. */
    fun name(): String? = query(uri).firstOrNull()?.name

    fun children(): List<Child> = query(DocumentsContract.buildChildDocumentsUriUsingTree(tree, id))

    /** The folder `name` in it, if there. */
    fun find(name: String): Folder? =
        children().firstOrNull { it.dir && it.name == name }?.let { Folder(context, tree, it.id) }

    /** The folder `name` in it, made if not there. */
    fun dir(name: String): Folder = find(name) ?: run {
        val made = DocumentsContract.createDocument(resolver, uri, Document.MIME_TYPE_DIR, name)
            ?: throw IOException("the folder $name could not be made")
        Folder(context, tree, DocumentsContract.getDocumentId(made))
    }

    /**
     * `from` copied in as `name`, any file of that name replaced, and read
     * back as long as it: a file it could not write whole is an error.
     */
    fun write(name: String, from: File) {
        delete(name)
        val made = DocumentsContract.createDocument(resolver, uri, "application/octet-stream", name)
            ?: throw IOException("$name could not be made")
        val named = query(made).firstOrNull()
        if (named?.name != name) {
            // The provider named it otherwise: no file of ours under another name.
            DocumentsContract.deleteDocument(resolver, made)
            throw IOException("the folder names files otherwise: ${named?.name}")
        }
        val out = resolver.openOutputStream(made, "w") ?: throw IOException("$name cannot be written")
        out.use { o -> from.inputStream().use { it.copyTo(o) } }
        val written = query(made).firstOrNull()?.bytes
        if (written != null && written >= 0 && written != from.length()) {
            throw IOException("$name written in part: $written of ${from.length()} bytes")
        }
    }

    /** The file `child` of it copied `to`. */
    fun read(child: Child, to: File) {
        val input = resolver.openInputStream(DocumentsContract.buildDocumentUriUsingTree(tree, child.id))
            ?: throw IOException("${child.name} cannot be read")
        input.use { i -> to.outputStream().use { i.copyTo(it) } }
    }

    /** Every file `name` in it deleted. */
    fun delete(name: String) {
        for (c in children()) {
            if (!c.dir && c.name == name) {
                DocumentsContract.deleteDocument(resolver, DocumentsContract.buildDocumentUriUsingTree(tree, c.id))
            }
        }
    }

    private fun query(of: Uri): List<Child> {
        val columns = arrayOf(
            Document.COLUMN_DOCUMENT_ID,
            Document.COLUMN_DISPLAY_NAME,
            Document.COLUMN_SIZE,
            Document.COLUMN_MIME_TYPE,
        )
        val cursor = resolver.query(of, columns, null, null, null) ?: throw IOException("the folder cannot be read")
        return cursor.use { c ->
            buildList {
                while (c.moveToNext()) {
                    add(
                        Child(
                            id = c.getString(0),
                            name = c.getString(1) ?: "",
                            bytes = if (c.isNull(2)) -1 else c.getLong(2),
                            dir = c.getString(3) == Document.MIME_TYPE_DIR,
                        ),
                    )
                }
            }
        }
    }

    companion object {
        /** The folder the user chose, `tree`. */
        fun of(context: Context, tree: Uri): Folder =
            Folder(context, tree, DocumentsContract.getTreeDocumentId(tree))
    }
}
