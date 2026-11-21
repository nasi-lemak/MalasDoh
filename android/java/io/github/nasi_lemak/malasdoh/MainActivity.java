// MalasDoh's activity: Android's NativeActivity, which runs the Rust app
// (libmalasdoh_android.so), plus what NativeActivity lacks:
//  - a proper connection to the on-screen keyboard, so word suggestions,
//    swipe typing and composing input methods work (NativeActivity only
//    gets single key presses);
//  - the clipboard;
//  - the system's file picker.
// The Rust side calls showKeyboard, setClipboard, getClipboard and
// pickFiles, and gets what is typed and picked through the native methods
// below.
package io.github.nasi_lemak.malasdoh;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.provider.OpenableColumns;
import java.io.File;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.util.ArrayList;
import android.text.InputType;
import android.view.KeyEvent;
import android.view.View;
import android.view.ViewGroup;
import android.view.inputmethod.BaseInputConnection;
import android.view.inputmethod.EditorInfo;
import android.view.inputmethod.InputConnection;
import android.view.inputmethod.InputMethodManager;

public class MainActivity extends NativeActivity {
    static {
        // Already loaded by NativeActivity; this lets Java find the native
        // methods in it.
        System.loadLibrary("malasdoh_android");
    }

    /** What is being typed into: a text field, a password, a terminal. */
    static final int TEXT = 0, PASSWORD = 1, TERMINAL = 2;

    static native void nativeCommit(String text);

    static native void nativeComposing(String text);

    /** An Android key code (Enter, Del, ForwardDel, arrows…). */
    static native void nativeKey(int keyCode);

    /** The keyboard's select all, cut, copy or paste (android.R.id.…). */
    static native void nativeAction(int id);

    /** The files picked for the dialog with this id (none: cancelled). */
    static native void nativePicked(String id, String[] paths);

    private static final int PICK = 4711;
    private String pickId;

    private InputView input;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        input = new InputView(this);
        addContentView(input, new ViewGroup.LayoutParams(1, 1));
    }

    @Override
    protected void onDestroy() {
        android.util.Log.i("malasdoh", "activity destroyed (finishing: " + isFinishing() + ", changing configuration: "
                + isChangingConfigurations() + ")");
        stopService(new Intent(this, KeepAliveService.class));
        super.onDestroy();
        // The Rust app runs once per process: a new activity in this one would
        // have no app behind it (and Android would call it not responding).
        // What was open has been saved, as it is whenever the app is left.
        android.os.Process.killProcess(android.os.Process.myPid());
    }

    /** Called from Rust, on its thread. */
    public void showKeyboard(final boolean show, final int mode) {
        runOnUiThread(() -> {
            InputMethodManager imm = (InputMethodManager) getSystemService(Context.INPUT_METHOD_SERVICE);
            if (show) {
                if (input.mode != mode) {
                    input.mode = mode;
                    imm.restartInput(input);
                }
                input.requestFocus();
                imm.showSoftInput(input, 0);
            } else {
                input.finishComposing();
                imm.hideSoftInputFromWindow(input.getWindowToken(), 0);
            }
        });
    }

    /** Called from Rust. */
    public void setClipboard(final String text) {
        runOnUiThread(() -> {
            ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
            cm.setPrimaryClip(ClipData.newPlainText("MalasDoh", text));
        });
    }

    /** Called from Rust. */
    public String getClipboard() {
        ClipboardManager cm = (ClipboardManager) getSystemService(Context.CLIPBOARD_SERVICE);
        ClipData clip = cm.getPrimaryClip();
        if (clip == null || clip.getItemCount() == 0) return null;
        CharSequence text = clip.getItemAt(0).coerceToText(this);
        return text == null ? null : text.toString();
    }

    /**
     * Called from Rust when the number of open connections changes: while
     * there are any, a foreground service keeps the app running in the
     * background (with its notification).
     */
    public void keepAlive(final int connections) {
        runOnUiThread(() -> {
            Intent service = new Intent(this, KeepAliveService.class);
            try {
                if (connections > 0) {
                    if (Build.VERSION.SDK_INT >= 33 && checkSelfPermission(
                            "android.permission.POST_NOTIFICATIONS") != android.content.pm.PackageManager.PERMISSION_GRANTED) {
                        // Without it the service still runs; the notification is only hidden.
                        requestPermissions(new String[] {"android.permission.POST_NOTIFICATIONS"}, 1);
                    }
                    service.putExtra(KeepAliveService.COUNT, connections);
                    startForegroundService(service);
                } else {
                    stopService(service);
                }
            } catch (RuntimeException e) {
                // Android may refuse to start it from the background.
                android.util.Log.w("malasdoh", "could not keep the connections alive in the background", e);
            }
        });
    }

    /** Called from Rust: hand a file to another app. Returns why not, or null. */
    public String shareFile(final String path) {
        final Uri uri;
        try {
            uri = FilesProvider.uriFor(this, new File(path));
        } catch (java.io.IOException e) {
            return e.getMessage();
        }
        runOnUiThread(() -> {
            Intent send = new Intent(Intent.ACTION_SEND);
            send.setType(getContentResolver().getType(uri));
            send.putExtra(Intent.EXTRA_STREAM, uri);
            send.setClipData(ClipData.newRawUri(new File(path).getName(), uri));
            send.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
            startActivity(Intent.createChooser(send, null));
        });
        return null;
    }

    static native void nativeUnlocked(boolean ok);

    private static final int UNLOCK = 4712;

    /**
     * Called from Rust: ask for the fingerprint, face or the device's PIN
     * (the app lock). A device with no screen lock at all is let through:
     * there is nothing to check against.
     */
    public void unlock() {
        runOnUiThread(() -> {
            String title = "Unlock MalasDoh";
            if (Build.VERSION.SDK_INT >= 30) {
                int how = android.hardware.biometrics.BiometricManager.Authenticators.BIOMETRIC_STRONG
                        | android.hardware.biometrics.BiometricManager.Authenticators.DEVICE_CREDENTIAL;
                android.hardware.biometrics.BiometricManager bm =
                        getSystemService(android.hardware.biometrics.BiometricManager.class);
                if (bm.canAuthenticate(how) != android.hardware.biometrics.BiometricManager.BIOMETRIC_SUCCESS) {
                    nativeUnlocked(true);
                    return;
                }
                new android.hardware.biometrics.BiometricPrompt.Builder(this).setTitle(title)
                        .setAllowedAuthenticators(how).build()
                        .authenticate(new android.os.CancellationSignal(), getMainExecutor(), prompt());
            } else if (Build.VERSION.SDK_INT >= 29) {
                android.app.KeyguardManager km = getSystemService(android.app.KeyguardManager.class);
                if (!km.isDeviceSecure()) {
                    nativeUnlocked(true);
                    return;
                }
                new android.hardware.biometrics.BiometricPrompt.Builder(this).setTitle(title)
                        .setDeviceCredentialAllowed(true).build()
                        .authenticate(new android.os.CancellationSignal(), getMainExecutor(), prompt());
            } else {
                android.app.KeyguardManager km =
                        (android.app.KeyguardManager) getSystemService(Context.KEYGUARD_SERVICE);
                Intent ask = km.createConfirmDeviceCredentialIntent(title, null);
                if (ask == null) {
                    nativeUnlocked(true);
                } else {
                    startActivityForResult(ask, UNLOCK);
                }
            }
        });
    }

    private android.hardware.biometrics.BiometricPrompt.AuthenticationCallback prompt() {
        return new android.hardware.biometrics.BiometricPrompt.AuthenticationCallback() {
            @Override
            public void onAuthenticationSucceeded(android.hardware.biometrics.BiometricPrompt.AuthenticationResult r) {
                nativeUnlocked(true);
            }

            @Override
            public void onAuthenticationError(int code, CharSequence message) {
                nativeUnlocked(false);
            }
        };
    }

    /** Called from Rust: open the system's file picker. */
    public void pickFiles(final String id, final boolean multiple) {
        runOnUiThread(() -> {
            pickId = id;
            Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
            intent.addCategory(Intent.CATEGORY_OPENABLE);
            intent.setType("*/*");
            intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, multiple);
            try {
                startActivityForResult(intent, PICK);
            } catch (RuntimeException e) {
                nativePicked(id, new String[0]);
            }
        });
    }

    @Override
    protected void onActivityResult(int request, int result, Intent data) {
        super.onActivityResult(request, result, data);
        if (request == UNLOCK) {
            nativeUnlocked(result == RESULT_OK);
            return;
        }
        if (request != PICK || pickId == null) return;
        final String id = pickId;
        pickId = null;
        final ArrayList<Uri> uris = new ArrayList<>();
        if (result == RESULT_OK && data != null) {
            if (data.getClipData() != null) {
                for (int i = 0; i < data.getClipData().getItemCount(); i++) {
                    uris.add(data.getClipData().getItemAt(i).getUri());
                }
            } else if (data.getData() != null) {
                uris.add(data.getData());
            }
        }
        // The app reads files by path: copy the picked ones into its cache,
        // away from the UI thread.
        new Thread(() -> {
            ArrayList<String> paths = new ArrayList<>();
            File dir = new File(getCacheDir(), "picked/" + System.currentTimeMillis());
            dir.mkdirs();
            for (Uri uri : uris) {
                File out = new File(dir, displayName(uri));
                try (InputStream in = getContentResolver().openInputStream(uri);
                     OutputStream o = new FileOutputStream(out)) {
                    byte[] buf = new byte[65536];
                    for (int n; (n = in.read(buf)) > 0; ) o.write(buf, 0, n);
                    paths.add(out.getAbsolutePath());
                } catch (Exception e) {
                    android.util.Log.w("malasdoh", "could not read " + uri, e);
                }
            }
            nativePicked(id, paths.toArray(new String[0]));
        }).start();
    }

    private String displayName(Uri uri) {
        try (Cursor c = getContentResolver().query(uri, new String[] {OpenableColumns.DISPLAY_NAME}, null, null,
                null)) {
            if (c != null && c.moveToFirst()) {
                String name = c.getString(0);
                if (name != null && !name.isEmpty() && !name.contains("/")) return name;
            }
        } catch (Exception ignored) {
        }
        return "picked";
    }

    /** A view the keyboard types into; everything goes to the Rust app. */
    static class InputView extends View {
        int mode = TEXT;
        private String composing = "";

        InputView(Context context) {
            super(context);
            setFocusable(true);
            setFocusableInTouchMode(true);
        }

        @Override
        public boolean onCheckIsTextEditor() {
            return true;
        }

        void finishComposing() {
            if (!composing.isEmpty()) {
                nativeCommit(composing);
                composing = "";
            }
        }

        @Override
        public InputConnection onCreateInputConnection(EditorInfo info) {
            switch (mode) {
                case PASSWORD:
                    info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_VARIATION_PASSWORD;
                    break;
                case TERMINAL:
                    // Every key straight through: no suggestions, no autocorrect.
                    info.inputType = InputType.TYPE_CLASS_TEXT
                            | InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD
                            | InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS;
                    break;
                default:
                    info.inputType = InputType.TYPE_CLASS_TEXT | InputType.TYPE_TEXT_FLAG_AUTO_CORRECT;
            }
            info.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN | EditorInfo.IME_FLAG_NO_EXTRACT_UI
                    | EditorInfo.IME_ACTION_NONE;
            composing = "";
            return new BaseInputConnection(this, false) {
                @Override
                public boolean commitText(CharSequence text, int newCursorPosition) {
                    android.util.Log.v("malasdoh", "keyboard: commit " + text.length() + " chars");
                    composing = "";
                    nativeCommit(text.toString());
                    return true;
                }

                // Select all, cut, copy and paste from the keyboard's
                // toolbar, or Ctrl+A/X/C/V on a keyboard it handles.
                @Override
                public boolean performContextMenuAction(int id) {
                    android.util.Log.v("malasdoh", "keyboard: action " + id);
                    nativeAction(id);
                    return true;
                }

                @Override
                public boolean setComposingText(CharSequence text, int newCursorPosition) {
                    composing = text.toString();
                    nativeComposing(composing);
                    return true;
                }

                @Override
                public boolean finishComposingText() {
                    finishComposing();
                    return true;
                }

                @Override
                public boolean deleteSurroundingText(int before, int after) {
                    android.util.Log.v("malasdoh", "keyboard: delete " + before + " before, " + after + " after");
                    for (int i = 0; i < before; i++) nativeKey(KeyEvent.KEYCODE_DEL);
                    for (int i = 0; i < after; i++) nativeKey(KeyEvent.KEYCODE_FORWARD_DEL);
                    return true;
                }

                @Override
                public boolean sendKeyEvent(KeyEvent event) {
                    if (event.getAction() != KeyEvent.ACTION_DOWN) return true;
                    android.util.Log.v("malasdoh", "keyboard: key " + event.getKeyCode());
                    int code = event.getKeyCode();
                    int ch = event.getUnicodeChar();
                    if (code == KeyEvent.KEYCODE_ENTER || code == KeyEvent.KEYCODE_DEL
                            || code == KeyEvent.KEYCODE_FORWARD_DEL || code == KeyEvent.KEYCODE_TAB
                            || code == KeyEvent.KEYCODE_ESCAPE || (code >= KeyEvent.KEYCODE_DPAD_UP
                            && code <= KeyEvent.KEYCODE_DPAD_RIGHT)) {
                        nativeKey(code);
                    } else if (ch != 0) {
                        nativeCommit(new String(Character.toChars(ch)));
                    }
                    return true;
                }
            };
        }
    }
}
