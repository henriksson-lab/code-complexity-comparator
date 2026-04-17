#include <vector>
#include <string>

class Counter {
public:
    Counter() : n_(0) {}
    int inc(int by) {
        if (by < 0) throw std::runtime_error("neg");
        n_ += by;
        return n_;
    }
private:
    int n_;
};

template<typename T>
T clamp_value(T v, T lo, T hi) {
    if (v < lo) return lo;
    if (v > hi) return hi;
    return v;
}

int process(const std::vector<int>& xs) {
    int total = 0;
    for (auto x : xs) {
        if (x > 0) total += x;
        else if (x < -100) total -= 100;
    }
    return total;
}
