/* Test PID 1 retains the normal identity protocol and runs one bounded
 * guest-side read/write check when its host publishes the marker bundle. */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

static int only_entry(const char *dir, const char *wanted)
{
    DIR *d = opendir(dir);
    if (!d) return 0;
    int count = 0;
    struct dirent *e;
    while ((e = readdir(d))) {
        if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, "..")) continue;
        if (strcmp(e->d_name, wanted)) { count = -1; break; }
        count++;
    }
    closedir(d);
    return count == 1;
}

static int check(void)
{
    char line[4096], own[37], boot[37], other[37], marker[37], path[160];
    FILE *f = fopen("/proc/cmdline", "r");
    if (!f) return 0;
    int ok = fgets(line, sizeof(line), f) != NULL;
    fclose(f);
    char *id = ok ? strstr(line, "nautilus.sandbox_id=") : NULL;
    if (!id || sscanf(id, "nautilus.sandbox_id=%36[0-9a-f-]", own) != 1) return 0;
    if (!only_entry("/probe", "containers") || !only_entry("/probe/containers", own)) return 0;
    snprintf(path, sizeof(path), "/probe/containers/%s/marker", own);
    f = fopen(path, "r");
    if (!f) return 0;
    ok = fscanf(f, "%36[0-9a-f-] %36[0-9a-f-]", other, marker) == 2;
    fclose(f);
    if (!ok || !strcmp(own, other)) return 0;
    snprintf(path, sizeof(path), "/probe/containers/%s", other);
    if (access(path, F_OK) == 0 || errno != ENOENT) return 0;
    f = fopen("/proc/sys/kernel/random/boot_id", "r");
    if (!f) return 0;
    ok = fscanf(f, "%36[0-9a-f-]", boot) == 1;
    fclose(f);
    if (!ok) return 0;
    snprintf(path, sizeof(path), "/probe/containers/%s/observed", own);
    f = fopen(path, "w");
    if (!f) return 0;
    ok = fprintf(f, "%s %s\n", boot, marker) > 0 && fflush(f) == 0 && fsync(fileno(f)) == 0;
    return fclose(f) == 0 && ok;
}

int main(void)
{
    pid_t child = fork();
    if (child < 0) return 1;
    if (!child) {
        if (mkdir("/probe", 0700)) return 2;
        for (int i = 0; i < 1000; ++i) {
            if (!mount("podfs", "/probe", "virtiofs", MS_NOSUID | MS_NODEV, NULL) || errno == EBUSY)
                break;
            usleep(50000);
        }
        for (int i = 0; i < 2000; ++i) {
            if (check()) return 0;
            usleep(50000);
        }
        return 3;
    }
    execl("/identity", "/identity", (char *)NULL);
    return 4;
}
