/* Test-only wrapper: report one late create failure and fail its first delete. */
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int once(const char *path)
{
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0)
        return 0;
    close(fd);
    return 1;
}

int main(int argc, char **argv)
{
    const char id[] = "00ca1100-1000-4000-8000-000000000001";
    if (argc > 2 && !strcmp(argv[argc - 1], id)) {
        for (int i = 1; i < argc; ++i) {
            if (!strcmp(argv[i], "delete") && once("/run/failed-delete"))
                return 32;
            if (!strcmp(argv[i], "create") && once("/run/failed-create")) {
                pid_t p = fork();
                if (p < 0)
                    return 1;
                if (p == 0)
                    break;
                int status;
                if (waitpid(p, &status, 0) != p || !WIFEXITED(status))
                    return 1;
                return WEXITSTATUS(status) == 0 ? 31 : WEXITSTATUS(status);
            }
        }
    }
    execv("/bin/runc-real", argv);
    perror("exec runc fixture");
    return 1;
}
