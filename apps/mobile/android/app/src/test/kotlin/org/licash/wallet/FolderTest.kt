package org.licash.wallet

import java.io.ByteArrayInputStream
import java.io.File
import java.io.IOException
import java.io.InputStream
import java.nio.file.Files
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test

class FolderTest {
    private fun scratch(): File = Files.createTempDirectory("folder").toFile().apply { deleteOnExit() }

    @Test fun aStreamOfAtMostTheLimitIsCopiedWhole() {
        val to = File(scratch(), "a.lcb")
        val bytes = ByteArray(20_000) { it.toByte() }
        assertTrue(copyAtMost(ByteArrayInputStream(bytes), to, bytes.size.toLong()))
        assertArrayEquals(bytes, to.readBytes())
        assertTrue(copyAtMost(ByteArrayInputStream(ByteArray(0)), to, 0))
        assertTrue(to.exists() && to.length() == 0L)
    }

    @Test fun aStreamPastTheLimitLeavesNothing() {
        val to = File(scratch(), "a.lcb")
        val bytes = ByteArray(20_001)
        assertFalse(copyAtMost(ByteArrayInputStream(bytes), to, 20_000))
        assertFalse(to.exists())
    }

    @Test fun anEndlessStreamIsReadNoFurtherThanPastTheLimit() {
        val to = File(scratch(), "a.lcb")
        var read = 0L
        val endless = object : InputStream() {
            override fun read(): Int = 0
            override fun read(b: ByteArray, off: Int, len: Int): Int {
                read += len
                return len
            }
        }
        assertFalse(copyAtMost(endless, to, 1_000_000))
        assertFalse(to.exists())
        assertTrue("read $read", read <= 1_000_000 + DEFAULT_BUFFER_SIZE)
    }

    @Test fun aStreamFailingPartwayLeavesNothing() {
        val to = File(scratch(), "a.lcb")
        val failing = object : InputStream() {
            var given = 0
            override fun read(): Int = 0
            override fun read(b: ByteArray, off: Int, len: Int): Int {
                if (given++ > 0) throw IOException("gone")
                return len
            }
        }
        try {
            copyAtMost(failing, to, 1_000_000)
            fail("a failing stream copied")
        } catch (_: IOException) {
        }
        assertFalse(to.exists())
    }
}
