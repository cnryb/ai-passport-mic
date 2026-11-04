// apmrun —— 稳定的启动器。
//
// 为什么是「读作业文件」而不是从 argv 拿参数：
//   经 LaunchServices（open）启动的 app，`--args` 并不保证转发到进程里（实测为空）。
//   所以参数一律写在固定路径的作业文件里，启动器自己读，链路最短也最可控。
//   作业文件格式（每行一项）：
//     第 1 行  apmctl 可执行文件路径
//     第 2 行  日志文件路径
//     第 3 行起 apmctl 的参数
//
// 为什么需要启动器：
//   macOS 的 TCC 按 app 身份授权，而每次重编 apmctl 并重签名，LaunchServices 就不肯再
//   启动那个 bundle。让一个签名永不变的薄启动器持有 app 身份，由它 exec apmctl，
//   并把子进程 stdout/stderr 重定向到日志文件。

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/wait.h>

#define JOB_FILE "/tmp/ai-passport-mic/job"

int main(void)
{
    FILE *job = fopen(JOB_FILE, "r");
    if (!job) {
        fprintf(stderr, "cannot open job file %s\n", JOB_FILE);
        return 2;
    }

    char *lines[64];
    size_t count = 0;
    char buf[4096];
    while (count < 64 && fgets(buf, sizeof(buf), job)) {
        size_t len = strlen(buf);
        while (len > 0 && (buf[len - 1] == '\n' || buf[len - 1] == '\r')) {
            buf[--len] = '\0';
        }
        lines[count++] = strdup(buf);
    }
    fclose(job);

    if (count < 2) {
        fprintf(stderr, "job file needs at least exe and log path\n");
        return 2;
    }

    const char *exe = lines[0];

    FILE *log = fopen(lines[1], "a");
    if (!log) {
        fprintf(stderr, "cannot open log %s\n", lines[1]);
        return 2;
    }

    // 组装子进程参数：argv[0] 必须是可执行文件本身
    char *child_argv[66];
    size_t n = 0;
    child_argv[n++] = (char *)exe;
    for (size_t i = 2; i < count; i++) {
        child_argv[n++] = lines[i];
    }
    child_argv[n] = NULL;

    pid_t pid = fork();
    if (pid < 0) {
        fprintf(stderr, "fork failed\n");
        return 2;
    }

    if (pid == 0) {
        dup2(fileno(log), STDOUT_FILENO);
        dup2(fileno(log), STDERR_FILENO);
        fclose(log);
        execv(exe, child_argv);
        fprintf(stderr, "exec %s failed\n", exe);
        _exit(127);
    }

    fclose(log);
    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        return 2;
    }
    return WIFEXITED(status) ? WEXITSTATUS(status) : 1;
}
