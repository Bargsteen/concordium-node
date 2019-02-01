# - Compile and run code to check for C++ features
#
# This functions compiles a source file under the `cmake` folder
# and adds the corresponding `HAVE_[FILENAME]` flag to the CMake
# environment
#
#  cxx_feature_check(<FLAG> [<VARIANT>])
#
# - Example
#
# include(CXXFeatureCheck)
# cxx_feature_check(STD_REGEX)
# Requires CMake 2.8.12+

if(__cxx_feature_check)
  return()
endif()
set(__cxx_feature_check INCLUDED)

function(cxx_feature_check FILE)
  string(TOLOWER ${FILE} FILE)
  string(TOUPPER ${FILE} VAR)
  string(TOUPPER "HAVE_${VAR}" FEATURE)
  SET( RUN_HAVE_STD_REGEX 0 CACHE STRING "Result from TRY_RUN" FORCE)
  SET( RUN_HAVE_STEADY_CLOCK 0 CACHE STRING "Result from TRY_RUN" FORCE)
  if (DEFINED HAVE_${VAR})
    set(HAVE_${VAR} 1 CACHE INTERNAL "Feature test for ${FILE}" PARENT_SCOPE)
    add_definitions(-DHAVE_${VAR})
    return()
  endif()
  message("-- Performing Test ${FEATURE}")
  try_run(RUN_${FEATURE} COMPILE_${FEATURE}
          ${CMAKE_BINARY_DIR} ${CMAKE_CURRENT_SOURCE_DIR}/cmake/${FILE}.cpp
          CMAKE_FLAGS ${BENCHMARK_CXX_LINKER_FLAGS}
          LINK_LIBRARIES ${BENCHMARK_CXX_LIBRARIES})
  if(RUN_${FEATURE} EQUAL 0)
    message("-- Performing Test ${FEATURE} -- success")
    set(HAVE_${VAR} 1 CACHE INTERNAL "Feature test for ${FILE}" PARENT_SCOPE)
    add_definitions(-DHAVE_${VAR})
  else()
    if(NOT COMPILE_${FEATURE})
      message("-- Performing Test ${FEATURE} -- failed to compile")
    else()
      message("-- Performing Test ${FEATURE} -- compiled but failed to run")
    endif()
  endif()
endfunction()

