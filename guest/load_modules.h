/* Load the guest modules in the dependency order recorded by the initramfs builder. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

static int load_modules(void)
{
    for (unsigned i = 0; ; i++) {
        char path[48];
        snprintf(path, sizeof(path), "/modules/%u.ko", i);
        int fd = open(path, O_RDONLY);
        if (fd < 0)
            return errno == ENOENT ? 0 : -1;
        int rc = syscall(SYS_finit_module, fd, "", 0);
        int err = errno;
        close(fd);
        if (rc < 0 && err != EEXIST) {
            errno = err;
            perror(path);
            return -1;
        }
    }
}
