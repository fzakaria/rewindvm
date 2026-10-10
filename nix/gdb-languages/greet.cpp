// Prints from Greeter::greet a few calls deep, with a vector and a string
// for gdb's printers. A worker thread sets its own copy of a thread-local
// to 7 and blocks, so gdb reads that thread's copy while it is off the
// CPU, and the main thread's is 1 at its first print.
#include <atomic>
#include <condition_variable>
#include <iostream>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

thread_local int calls = 0;

std::mutex lock;
std::condition_variable woken;
std::atomic<bool> ready{false};
bool done = false;

struct Greeter {
  std::string name;
  std::vector<int> counts;

  void greet(const std::string &who) {
    calls++;
    counts.push_back(static_cast<int>(who.size()));
    std::cout << "hello from c++, " << who << " " << counts.size() << std::endl;
  }
};

int main() {
  std::thread worker([] {
    calls = 7;
    ready = true;
    std::unique_lock<std::mutex> held(lock);
    woken.wait(held, [] { return done; });
  });
  while (!ready) {
    std::this_thread::yield();
  }

  Greeter g{"cpp", {1, 2, 3}};
  for (const char *who : {"alice", "bob"}) {
    g.greet(who);
  }

  {
    std::lock_guard<std::mutex> held(lock);
    done = true;
  }
  woken.notify_one();
  worker.join();
}
