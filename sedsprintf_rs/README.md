# SEDSPRINTF_RS

An implementation of the `sedsprintf` library in Rust.

## Overview

This library provides a safe and efficient way to handle telemetry data including serializing, routing, and converting
into strings for logging. The purpose of this library is to provide an easy and consistent way to create and transport
any data to any destination without needing to implement the core routing on every platform, the library handles the
creation and routing of the data while allowing for easy extension to support new data types and destinations. The user
of the library is responsible for implementing 2 core functions and one function per local data endpoint.
The core functions are as follows:

- A function to send raw bytes to the all other nodes (e.g. UART, SPI, I2C, CAN, etc.)
- A function to receive raw bytes from all other nodes and passes the packet to the router
  (e.g. UART, SPI, I2C, CAN, etc.)
- A function to handle local data endpoints (e.g. logging to console, writing to file, sending over radio, etc.)
  (Note: each local endpoint needs its own function)

The library also provides helpers to convert the telemetry data into strings for logging purposes. the library also
handles the serialization and deserialization of the telemetry data.

The library is platform-agnostic and can be used on any platform that supports Rust. The library is primarily designed
to be used in embedded systems and used by a C program, but can also be used in desktop applications and other rust
codebases.

This library also supports python bindings via pyo3. to use you need maturin installed to build the python package.
The size of the header in a serialized packet is around 20 bytes (the size will change based on the total number of
endpoints in your system and the length of the sender string), so a packet containing three floats is 32 bytes total.
This small size makes it ideal for use in low bandwidth environments.

## Building

To build the library in a C project, just include the library as a submodule or subtree and link it in your
cmakelists.txt as shown below.
For other build systems, you can build the library as a static or dynamic library using cargo and link it to your
project.

Building with python bindings can be done with the build script on posix systems:

```bash
./build.py release maturin-develop 
```

When building in an embedded environment the library will compile to a static library that can be linked to your C code.
this library takes up about 100kb of flash and does require heap allocation to be available through either freertos, or
by creating shims that expose pvPortMalloc and vPortFree.

## Dependencies

- Rust

  get it from https://rustup.rs/
- Cmake
- A C++ compiler
- A C compiler

## Usage

- When using this library as a submodule or subtree in a C or C++ project, make sure to add the following to your
  cmakelists.txt and adjust the target as needed:
  ```cmake
  set(SEDSPRINTF_RS_TARGET "thumbv7m-none-eabi" CACHE STRING "" FORCE) # Set target for embedded systems
  add_subdirectory(${CMAKE_SOURCE_DIR}/sedsprintf_rs)
  target_link_libraries(${CMAKE_PROJECT_NAME} sedsprintf_rs)
  ```
- Setup the config.rs to match your application needs. All config options are in the config.rs file and are very
  self-explanatory.
  NOTE: (ON EVERY SYSTEM THIS LIBRARY IS USED, THE CONFIG ENUMS MUST BE THE SAME OR UNDEFINED BEHAVIOR MAY OCCUR). So
  for most
  applications I would recommend making a fork and setting the config values you need for your application minus the
  sender,
  and then only changing the sender string on each system, this will ensure that the enum values are the same on all
  systems.


- To add this repo as a subtree to allow for modifications, use the following command:
  ```bash
  git remote add sedsprintf-upstream https://github.com/University-at-Buffalo-SEDS/sedsprintf_2026.git
  git fetch sedsprintf-upstream
  
  git config subtree.sedsprintf_rs.remote sedsprintf-upstream
  git config subtree.sedsprintf_rs.branch dev   # or main or the branch of your choosing

  git subtree add --prefix=sedsprintf_rs sedsprintf-upstream main
  ```
  To switch branches, run the following
  ```bash
  git config subtree.sedsprintf_rs.branch <the-new-branch> 
  ```
- To update the subtree, use the following command (Note all local changes must be committed before you can update):
  ```bash
  git subtree pull --prefix=sedsprintf_rs sedsprintf-upstream main -m "Merge sedsprintf_rs upstream main"
  ```


- If using in an embedded environment, make sure to provide the necessary allocation functions if using a custom
  allocator.
  Below is an example implementation using malloc, free, and fwrite, feel free to implement to use your own allocator or
  use ones provided by your RTOS. In the same sense, feel free to implement your own logging mechanism for when the
  telemetry falls back to local logging for errors.


```C
// telemetry_hooks.c
#include <stddef.h>
#include <stdlib.h>

/*
 * Rust expects these functions to exist for heap allocations:
 *
 *   void *telemetryMalloc(size_t);
 *   void telemetryFree(void *);
 *   void seds_error_msg(const char *, const size_t);
 *
 */

void *telemetryMalloc(size_t xSize)
{
     return malloc(xSize);
}

void telemetryFree(void *pv)
{
    free(pv);
}

void seds_error_msg(const char *str, const size_t len)
{
    // Implement your logging mechanism here, for example:
    fwrite(str, 1, len, stderr);
    fwrite("\n", 1, 1, stderr);
}
```
