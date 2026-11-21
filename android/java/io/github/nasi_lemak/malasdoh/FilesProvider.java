// Lets another app read a file MalasDoh shares with it (Share… on a file
// in "This device"), and only that file, only while the share lasts: the
// receiving app gets a content:// address and a read grant, as Android
// requires (it no longer allows passing file paths).
package io.github.nasi_lemak.malasdoh;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import android.provider.OpenableColumns;
import android.webkit.MimeTypeMap;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.IOException;

public class FilesProvider extends ContentProvider {
    static final String AUTHORITY = "io.github.nasi_lemak.malasdoh.files";

    /** The address of `file`, which must be in the app's own storage. */
    static Uri uriFor(android.content.Context context, File file) throws IOException {
        String root = context.getFilesDir().getCanonicalPath();
        String path = file.getCanonicalPath();
        if (!path.startsWith(root + "/")) throw new IOException("not in the app's storage: " + path);
        return new Uri.Builder().scheme("content").authority(AUTHORITY)
                .encodedPath(Uri.encode(path.substring(root.length()), "/")).build();
    }

    private File fileFor(Uri uri) throws FileNotFoundException {
        try {
            File root = getContext().getFilesDir().getCanonicalFile();
            File file = new File(root, uri.getPath()).getCanonicalFile();
            if (!file.getPath().startsWith(root.getPath() + "/")) throw new FileNotFoundException(uri.toString());
            return file;
        } catch (IOException e) {
            throw new FileNotFoundException(uri.toString());
        }
    }

    @Override
    public boolean onCreate() {
        return true;
    }

    @Override
    public ParcelFileDescriptor openFile(Uri uri, String mode) throws FileNotFoundException {
        return ParcelFileDescriptor.open(fileFor(uri), ParcelFileDescriptor.MODE_READ_ONLY);
    }

    @Override
    public String getType(Uri uri) {
        String ext = MimeTypeMap.getFileExtensionFromUrl(uri.toString());
        String type = ext == null ? null : MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext.toLowerCase());
        return type != null ? type : "application/octet-stream";
    }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String sort) {
        File file;
        try {
            file = fileFor(uri);
        } catch (FileNotFoundException e) {
            return null;
        }
        MatrixCursor c = new MatrixCursor(new String[] {OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE});
        c.addRow(new Object[] {file.getName(), file.length()});
        return c;
    }

    @Override
    public Uri insert(Uri uri, ContentValues values) {
        throw new UnsupportedOperationException("read only");
    }

    @Override
    public int delete(Uri uri, String selection, String[] args) {
        throw new UnsupportedOperationException("read only");
    }

    @Override
    public int update(Uri uri, ContentValues values, String selection, String[] args) {
        throw new UnsupportedOperationException("read only");
    }
}
