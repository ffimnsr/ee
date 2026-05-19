#include <chrono>
#include <csignal>
#include <exception>
#include <fstream>
#include <functional>
#include <iomanip>
#include <iostream>
#include <memory>
#include <optional>
#include <sstream>
#include <stdexcept>
#include <string>
#include <thread>
#include <vector>

// -----------------------------
// Configuration & Types
// -----------------------------

struct HelloWorldConfig {
    std::string prefix           = ">>> ";
    std::string message          = "Hello, World";
    std::string suffix           = " <<<";
    bool        enableTimestamp  = true;
    bool        uppercase        = false;
    int         repeatCount      = 3;
    bool        logToFile        = false;
    std::string logFilePath      = "hello.log";
    bool        simulateWorkload = true;
    int         workloadChunks   = 3;
    int         workloadDelayMs  = 300;
};

class ConfigError : public std::runtime_error {
public:
    explicit ConfigError(const std::string& msg)
        : std::runtime_error("ConfigError: " + msg) {}
};

// -----------------------------
// Utility helpers
// -----------------------------

std::string toUpperCopy(const std::string& in) {
    std::string out;
    out.reserve(in.size());
    for (unsigned char c : in) {
        out.push_back(static_cast<char>(std::toupper(c)));
    }
    return out;
}

std::string currentTimestampISO8601() {
    using namespace std::chrono;
    auto now     = system_clock::now();
    auto nowTime = system_clock::to_time_t(now);

    std::tm tm{};
#if defined(_WIN32)
    localtime_s(&tm, &nowTime);
#else
    localtime_r(&nowTime, &tm);
#endif

    std::ostringstream oss;
    oss << std::put_time(&tm, "%Y-%m-%dT%H:%M:%S");

    auto ms = duration_cast<milliseconds>(now.time_since_epoch()) % 1000;
    oss << "." << std::setw(3) << std::setfill('0') << ms.count();

    // No timezone offset handling here, just pretend it's local
    return oss.str();
}

// -----------------------------
// Validation
// -----------------------------

void validateConfig(const HelloWorldConfig& cfg) {
    if (cfg.repeatCount < 1) {
        throw ConfigError("repeatCount must be >= 1");
    }
    if (cfg.logToFile && cfg.logFilePath.empty()) {
        throw ConfigError("logFilePath required when logToFile is true");
    }
    if (cfg.workloadChunks < 0) {
        throw ConfigError("workloadChunks must be >= 0");
    }
    if (cfg.workloadDelayMs < 0) {
        throw ConfigError("workloadDelayMs must be >= 0");
    }
}

// -----------------------------
// Message builder
// -----------------------------

std::string buildMessage(const HelloWorldConfig& cfg) {
    std::string msg = cfg.prefix + cfg.message + cfg.suffix;

    if (cfg.uppercase) {
        msg = toUpperCopy(msg);
    }

    if (cfg.enableTimestamp) {
        msg = "[" + currentTimestampISO8601() + "] " + msg;
    }

    return msg;
}

// -----------------------------
// Workload simulator
// -----------------------------

class WorkloadSimulator {
public:
    explicit WorkloadSimulator(const HelloWorldConfig& cfg)
        : cfg_(cfg) {}

    void run() const {
        if (!cfg_.simulateWorkload || cfg_.workloadChunks == 0) {
            return;
        }

        for (int i = 0; i < cfg_.workloadChunks; ++i) {
            std::cout << "Processing chunk " << (i + 1)
                      << "/" << cfg_.workloadChunks << "...\n";
            std::this_thread::sleep_for(
                std::chrono::milliseconds(cfg_.workloadDelayMs));
        }
    }

private:
    const HelloWorldConfig& cfg_;
};

// -----------------------------
// Logger with RAII file handle
// -----------------------------

class Logger {
public:
    explicit Logger(const HelloWorldConfig& cfg)
        : cfg_(cfg) {
        if (cfg_.logToFile) {
            file_.emplace(cfg_.logFilePath, std::ios::app);
            if (!file_->is_open()) {
                throw std::runtime_error("Failed to open log file: " + cfg_.logFilePath);
            }
        }
    }

    void log(const std::string& msg) {
        if (file_) {
            (*file_) << msg << '\n';
            file_->flush();
        }
    }

private:
    const HelloWorldConfig& cfg_;
    std::optional<std::ofstream> file_;
};

// -----------------------------
// Signal handling (overkill)
// -----------------------------

namespace {
    std::atomic<bool> g_shouldStop{false};

    void signalHandler(int signum) {
        std::cerr << "\nReceived signal " << signum
                  << ", attempting graceful shutdown...\n";
        g_shouldStop.store(true);
    }

    void installSignalHandlers() {
        std::signal(SIGINT, signalHandler);
#if !defined(_WIN32)
        std::signal(SIGTERM, signalHandler);
#endif
    }
}

// -----------------------------
// The over-engineered Hello World
// -----------------------------

void helloWorld(const HelloWorldConfig& cfg) {
    validateConfig(cfg);

    installSignalHandlers();

    WorkloadSimulator simulator(cfg);
    simulator.run();

    Logger logger(cfg);
    const std::string msg = buildMessage(cfg);

    for (int i = 0; i < cfg.repeatCount; ++i) {
        if (g_shouldStop.load()) {
            std::cerr << "Stopping early at iteration " << i << "\n";
            break;
        }

        std::cout << msg << std::endl;
        logger.log(msg);

        // Pretend each print is heavy
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
}

// -----------------------------
// main
// -----------------------------

int main() {
    try {
        HelloWorldConfig cfg;
        cfg.prefix           = ">>> ";
        cfg.message          = "Hello, C++ World";
        cfg.suffix           = " <<<";
        cfg.enableTimestamp  = true;
        cfg.uppercase        = false;
        cfg.repeatCount      = 5;
        cfg.logToFile        = false;
        cfg.logFilePath      = "hello_cpp.log";
        cfg.simulateWorkload = true;
        cfg.workloadChunks   = 4;
        cfg.workloadDelayMs  = 250;

        helloWorld(cfg);
    } catch (const ConfigError& e) {
        std::cerr << "Configuration error: " << e.what() << "\n";
        return 1;
    } catch (const std::exception& e) {
        std::cerr << "Unhandled exception: " << e.what() << "\n";
        return 2;
    } catch (...) {
        std::cerr << "Unknown fatal error.\n";
        return 3;
    }

    return 0;
}
