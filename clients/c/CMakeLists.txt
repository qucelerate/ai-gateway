cmake_minimum_required(VERSION 3.16)
# The version is gateway.h's GW_VERSION_STRING, its one source.
file(STRINGS include/gateway/gateway.h GATEWAY_VERSION_LINE REGEX "^#define GW_VERSION_STRING ")
string(REGEX MATCH "[0-9]+\\.[0-9]+\\.[0-9]+" GATEWAY_VERSION "${GATEWAY_VERSION_LINE}")
project(qucelerate-ai-gateway VERSION ${GATEWAY_VERSION} LANGUAGES C CXX)
include(GNUInstallDirs)

option(GATEWAY_BUILD_TESTS "Build the tests and the conformance harness" ON)
option(GATEWAY_SANITIZE "Build with AddressSanitizer and UndefinedBehaviorSanitizer" OFF)
# CI turns warnings into errors; a release build never fails over a new compiler's new warning.
option(GATEWAY_WERROR "Treat warnings as errors in this library's own sources" OFF)
set(GATEWAY_WARNINGS -Wall -Wextra -Wpedantic -Wshadow -Wconversion -Wno-sign-conversion)
if(GATEWAY_WERROR)
  list(APPEND GATEWAY_WARNINGS -Werror)
endif()

# 7.85 adds CURLOPT_PROTOCOLS_STR, which restricts transfers to http(s).
find_package(CURL 7.85 REQUIRED)

if(GATEWAY_SANITIZE)
  add_compile_options(-fsanitize=address,undefined -fno-omit-frame-pointer -fno-sanitize-recover=all)
  add_link_options(-fsanitize=address,undefined)
endif()

set(GATEWAY_SOURCES src/common.c src/sse.c src/objects.c src/decode.c src/client.c src/batches.c)

add_library(gateway_objects OBJECT ${GATEWAY_SOURCES} src/json.c)
set_target_properties(gateway_objects PROPERTIES
  C_STANDARD 11
  C_STANDARD_REQUIRED ON
  C_EXTENSIONS OFF
  C_VISIBILITY_PRESET hidden
  POSITION_INDEPENDENT_CODE ON)
target_include_directories(gateway_objects PUBLIC include)
target_compile_definitions(gateway_objects PRIVATE GW_BUILDING)
target_link_libraries(gateway_objects PUBLIC CURL::libcurl)
if(CMAKE_C_COMPILER_ID MATCHES "GNU|Clang")
  # Warnings for this library's own sources, not the vendored JSON parser.
  set_source_files_properties(${GATEWAY_SOURCES} PROPERTIES
    COMPILE_OPTIONS "${GATEWAY_WARNINGS}")
endif()

# libqucelerate-ai-gateway.so.0: only the GW_API functions of gateway.h are
# exported. Installed, the target is qucelerate::ai-gateway.
add_library(gateway SHARED $<TARGET_OBJECTS:gateway_objects>)
set_target_properties(gateway PROPERTIES VERSION ${PROJECT_VERSION} SOVERSION ${PROJECT_VERSION_MAJOR}
  OUTPUT_NAME qucelerate-ai-gateway EXPORT_NAME ai-gateway)
target_include_directories(gateway PUBLIC $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/include>
                                          $<INSTALL_INTERFACE:${CMAKE_INSTALL_INCLUDEDIR}>)
target_link_libraries(gateway PRIVATE CURL::libcurl)

# The tests link the objects statically to reach the internal parser.
add_library(gateway_static STATIC $<TARGET_OBJECTS:gateway_objects>)
target_link_libraries(gateway_static PUBLIC gateway_objects)

set(GATEWAY_STRICT "")
if(CMAKE_CXX_COMPILER_ID MATCHES "GNU|Clang")
  set(GATEWAY_STRICT ${GATEWAY_WARNINGS})
endif()

add_executable(gateway_chat examples/chat.cpp)
target_compile_features(gateway_chat PRIVATE cxx_std_17)
target_compile_options(gateway_chat PRIVATE ${GATEWAY_STRICT})
target_link_libraries(gateway_chat PRIVATE gateway)

# Installs libqucelerate-ai-gateway.so.0, the headers (<gateway/gateway.h>),
# a CMake package (find_package(qucelerate-ai-gateway) gives
# qucelerate::ai-gateway) and qucelerate-ai-gateway.pc for pkg-config.
include(CMakePackageConfigHelpers)
install(TARGETS gateway EXPORT gatewayTargets
  LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}
  ARCHIVE DESTINATION ${CMAKE_INSTALL_LIBDIR}
  RUNTIME DESTINATION ${CMAKE_INSTALL_BINDIR})
install(DIRECTORY include/gateway DESTINATION ${CMAKE_INSTALL_INCLUDEDIR})
install(EXPORT gatewayTargets NAMESPACE qucelerate:: FILE qucelerate-ai-gatewayTargets.cmake
  DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/qucelerate-ai-gateway)
file(WRITE ${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gatewayConfig.cmake
  "include(\"\${CMAKE_CURRENT_LIST_DIR}/qucelerate-ai-gatewayTargets.cmake\")\n")
write_basic_package_version_file(${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gatewayConfigVersion.cmake
  COMPATIBILITY SameMinorVersion)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gatewayConfig.cmake
  ${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gatewayConfigVersion.cmake
  DESTINATION ${CMAKE_INSTALL_LIBDIR}/cmake/qucelerate-ai-gateway)
# Relocatable: the .pc file finds the prefix from its own directory, so an
# install with cmake --install --prefix, or a moved tree, still resolves.
file(RELATIVE_PATH GATEWAY_PC_TO_PREFIX "/prefix/${CMAKE_INSTALL_LIBDIR}/pkgconfig" "/prefix")
configure_file(qucelerate-ai-gateway.pc.in ${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gateway.pc @ONLY)
install(FILES ${CMAKE_CURRENT_BINARY_DIR}/qucelerate-ai-gateway.pc DESTINATION ${CMAKE_INSTALL_LIBDIR}/pkgconfig)
install(FILES LICENSE DESTINATION ${CMAKE_INSTALL_DATADIR}/doc/qucelerate-ai-gateway)
install(FILES third_party/yyjson/LICENSE DESTINATION ${CMAKE_INSTALL_DATADIR}/doc/qucelerate-ai-gateway
  RENAME LICENSE.yyjson)

if(GATEWAY_BUILD_TESTS)
  enable_testing()
  add_executable(test_c tests/test_c.c)
  set_target_properties(test_c PROPERTIES C_STANDARD 11)
  target_include_directories(test_c PRIVATE src)
  target_compile_options(test_c PRIVATE ${GATEWAY_STRICT})
  target_link_libraries(test_c PRIVATE gateway_static)
  add_test(NAME c_api COMMAND test_c)

  add_executable(conformance tests/conformance.cpp)
  target_compile_features(conformance PRIVATE cxx_std_17)
  target_include_directories(conformance PRIVATE src)
  target_compile_options(conformance PRIVATE ${GATEWAY_STRICT})
  target_link_libraries(conformance PRIVATE gateway_static)
  add_test(NAME conformance COMMAND conformance ${CMAKE_CURRENT_SOURCE_DIR}/../conformance)
endif()
