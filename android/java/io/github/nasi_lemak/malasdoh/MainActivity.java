// MalasDoh's activity: Android's NativeActivity, which runs the Rust app
// (libmalasdoh_android.so), plus what NativeActivity lacks:
//  - a proper connection to the on-screen keyboard, so word suggestions,
//    swipe typing and composing input methods work (NativeActivity only
//    gets single key presses);
//  - the clipboard.
// The Rust side calls showKeyboard, setClipboard and getClipboard, and gets
// what is typed through the native methods below.
package io.github.nasi_lemak.malasdoh;

import android.app.NativeActivity;
import android.content.ClipData;
import android.content.ClipboardManager;
import android.content.Context;
import android.os.Bundle;
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

    private InputView input;

    @Override
    protected void onCreate(Bundle state) {
        super.onCreate(state);
        input = new InputView(this);
        addContentView(input, new ViewGroup.LayoutParams(1, 1));
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
                    composing = "";
                    nativeCommit(text.toString());
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
                    for (int i = 0; i < before; i++) nativeKey(KeyEvent.KEYCODE_DEL);
                    for (int i = 0; i < after; i++) nativeKey(KeyEvent.KEYCODE_FORWARD_DEL);
                    return true;
                }

                @Override
                public boolean sendKeyEvent(KeyEvent event) {
                    if (event.getAction() != KeyEvent.ACTION_DOWN) return true;
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
