package com.zdroid

import android.content.Context
import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import android.util.Base64
import java.nio.ByteBuffer
import java.security.KeyStore
import java.security.MessageDigest
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec

/** App-private credential storage backed by a non-exportable Android Keystore key. */
internal object SecureCredentialStore {
    private const val KEY_ALIAS = "zdroid-credentials-v1"
    private const val PREFS_NAME = "zdroid-secure-credentials"
    private const val TRANSFORMATION = "AES/GCM/NoPadding"

    fun write(context: Context, url: String, username: String, password: ByteArray): Boolean {
        return runCatching {
            val usernameBytes = username.toByteArray(Charsets.UTF_8)
            val cleartext = ByteBuffer.allocate(4 + usernameBytes.size + password.size)
                .putInt(usernameBytes.size)
                .put(usernameBytes)
                .put(password)
                .array()

            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.ENCRYPT_MODE, getOrCreateKey())
            cipher.updateAAD(url.toByteArray(Charsets.UTF_8))
            val encrypted = cipher.doFinal(cleartext)
            val packed = ByteBuffer.allocate(4 + cipher.iv.size + encrypted.size)
                .putInt(cipher.iv.size)
                .put(cipher.iv)
                .put(encrypted)
                .array()

            context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .edit()
                .putString(preferenceKey(url), Base64.encodeToString(packed, Base64.NO_WRAP))
                .commit()
        }.getOrDefault(false)
    }

    fun read(context: Context, url: String): ByteArray? {
        return runCatching {
            val encoded = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
                .getString(preferenceKey(url), null) ?: return null
            val packed = ByteBuffer.wrap(Base64.decode(encoded, Base64.NO_WRAP))
            val ivLength = packed.int
            require(ivLength in 12..32 && packed.remaining() > ivLength)
            val iv = ByteArray(ivLength).also(packed::get)
            val encrypted = ByteArray(packed.remaining()).also(packed::get)

            val cipher = Cipher.getInstance(TRANSFORMATION)
            cipher.init(Cipher.DECRYPT_MODE, getOrCreateKey(), GCMParameterSpec(128, iv))
            cipher.updateAAD(url.toByteArray(Charsets.UTF_8))
            cipher.doFinal(encrypted)
        }.getOrNull()
    }

    fun delete(context: Context, url: String): Boolean {
        return context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            .edit()
            .remove(preferenceKey(url))
            .commit()
    }

    private fun getOrCreateKey(): SecretKey {
        val keyStore = KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
        (keyStore.getKey(KEY_ALIAS, null) as? SecretKey)?.let { return it }

        val generator = KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, "AndroidKeyStore")
        generator.init(
            KeyGenParameterSpec.Builder(
                KEY_ALIAS,
                KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
            )
                .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
                .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
                .setKeySize(256)
                .build()
        )
        return generator.generateKey()
    }

    private fun preferenceKey(url: String): String {
        val digest = MessageDigest.getInstance("SHA-256")
            .digest(url.toByteArray(Charsets.UTF_8))
        return Base64.encodeToString(digest, Base64.NO_WRAP or Base64.URL_SAFE)
    }
}
