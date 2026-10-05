/*
 * Example application-owned compatibility shim, compiled only when the
 * fixture's lstat audit opts in. ESP-IDF 6.1 VFS has no symbolic links, so
 * lstat can defer to stat. argyle-nimble does not provide this symbol: a
 * library defining a global POSIX function would override any future SDK or
 * application implementation, so the application owns the decision.
 */
#include <sys/stat.h>

int lstat(const char *path, struct stat *result)
{
    return stat(path, result);
}
