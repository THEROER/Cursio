package dev.cursio.mesh

import android.Manifest
import android.annotation.SuppressLint
import android.bluetooth.BluetoothClass
import android.bluetooth.BluetoothManager
import android.bluetooth.BluetoothSocket
import android.content.Context
import android.content.pm.PackageManager
import android.os.Build
import android.os.ParcelFileDescriptor
import java.io.FileInputStream
import java.io.FileOutputStream
import java.io.InputStream
import java.io.OutputStream
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * The phone's end of the mesh over Bluetooth: an RFCOMM socket to a paired
 * computer's Cursio service, and nothing spoken on it but the mesh.
 *
 * The node decides when to dial (`crates/android/src/bt.rs`); this only lists
 * the paired computers and opens the socket. Android gives an app an RFCOMM
 * socket only as Java streams, never as a descriptor, so the node hands over
 * one end of a socket pair and two threads here copy bytes between it and the
 * Bluetooth streams. The node then reads its own end like any socket, and
 * the handshake, Noise and every module run over it unchanged.
 */
object BtLink {
    /** Must match `cursio_bluetooth::MESH_UUID`, the computer's service. */
    private val MESH_UUID: UUID = UUID.fromString("8a3f6c2e-0b4d-4e8a-9c1f-43757273696f")

    lateinit var context: Context

    private fun allowed(): Boolean =
        Build.VERSION.SDK_INT < Build.VERSION_CODES.S ||
            context.checkSelfPermission(Manifest.permission.BLUETOOTH_CONNECT) ==
            PackageManager.PERMISSION_GRANTED

    private fun adapter() = context.getSystemService(BluetoothManager::class.java)?.adapter

    /**
     * Paired computers as `address\tname` lines. Headphones, watches and cars
     * are left out: they have no Cursio service, and asking each for one is
     * seconds of radio time per sweep.
     */
    @SuppressLint("MissingPermission")
    fun computers(): String {
        if (!allowed()) return ""
        val adapter = adapter() ?: return ""
        if (!adapter.isEnabled) return ""
        return try {
            adapter.bondedDevices
                .filter { it.bluetoothClass?.majorDeviceClass == BluetoothClass.Device.Major.COMPUTER }
                .joinToString("\n") { "${it.address}\t${it.name ?: it.address}" }
        } catch (e: SecurityException) {
            ""
        }
    }

    /**
     * Connect to [address]'s Cursio service and pump it through [fd], which
     * this takes ownership of: closed here on every way out, including a
     * refusal. Blocks until connected. Answers why it failed, or null.
     */
    @SuppressLint("MissingPermission")
    fun connect(address: String, fd: Int): String? {
        val pipe = ParcelFileDescriptor.adoptFd(fd)
        try {
            if (!allowed()) {
                pipe.close()
                return "the nearby-devices permission is not granted"
            }
            val adapter = adapter()
            if (adapter == null || !adapter.isEnabled) {
                pipe.close()
                return "bluetooth is off"
            }
            val socket = adapter.getRemoteDevice(address).createRfcommSocketToServiceRecord(MESH_UUID)
            socket.connect()
            pump(socket, pipe)
            return null
        } catch (e: Exception) {
            runCatching { pipe.close() }
            return e.message ?: e.toString()
        }
    }

    private fun pump(socket: BluetoothSocket, pipe: ParcelFileDescriptor) {
        val closed = AtomicBoolean(false)
        val close = {
            if (closed.compareAndSet(false, true)) {
                runCatching { socket.close() }
                runCatching { pipe.close() }
            }
        }
        val fromRadio = socket.inputStream
        val toRadio = socket.outputStream
        val fromNode = FileInputStream(pipe.fileDescriptor)
        val toNode = FileOutputStream(pipe.fileDescriptor)
        thread(name = "bt-in", isDaemon = true) { copy(fromRadio, toNode, close) }
        thread(name = "bt-out", isDaemon = true) { copy(fromNode, toRadio, close) }
    }

    /** Copy until either side ends, then end both. */
    private fun copy(from: InputStream, to: OutputStream, close: () -> Unit) {
        val buffer = ByteArray(16 * 1024)
        try {
            while (true) {
                val n = from.read(buffer)
                if (n < 0) break
                to.write(buffer, 0, n)
                to.flush()
            }
        } catch (_: Exception) {
        } finally {
            close()
        }
    }
}
