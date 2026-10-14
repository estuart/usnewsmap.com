/* Preloaded into the Tesseract engine image (#455): Tesseract's main() turns
 * on floating-point traps, and OpenJPEG's wavelet code then dies with SIGFPE
 * on JP2s it otherwise decodes. Keep traps off: report none enabled, enable
 * none. */
int feenableexcept(int excepts);

int feenableexcept(int excepts)
{
    (void)excepts;
    return 0;
}
