/* yyjson, with its symbols hidden: it is not part of this library's ABI. */
#ifndef GATEWAY_JSON_H
#define GATEWAY_JSON_H

#if !defined(yyjson_api)
#  if defined(__GNUC__) && !defined(_WIN32)
#    define yyjson_api __attribute__((visibility("hidden")))
#  else
#    define yyjson_api
#  endif
#endif
#include "../third_party/yyjson/yyjson.h"

#endif
