#import "Basic";
#import "POSIX";

libc :: #system_library "libc";
sleep :: (n: s64) -> void #foreign libc;

main :: () {
    pid := getpid();

    ndx := 0;
    while true {
        print("jai looping (pid %): %\n", pid, ndx);
        ndx += 1;
        sleep(1);
    }
}
