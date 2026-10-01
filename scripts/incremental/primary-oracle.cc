#include <libheif/heif.h>
#include <png.h>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <memory>
#include <stdexcept>
#include <string>
#include <vector>

void check(heif_error error) {
  if (error.code != heif_error_Ok) {
    throw std::runtime_error(error.message);
  }
}

int main(int argc, char **argv) {
  try {
    if (argc < 2 || argc > 4) {
      throw std::runtime_error("Usage: primary-oracle INPUT [PNG|-] [strict|recover]");
    }
    auto context = std::unique_ptr<heif_context, decltype(&heif_context_free)>(
        heif_context_alloc(), heif_context_free);
    if (!context) {
      throw std::runtime_error("Cannot allocate decoder context");
    }
    check(heif_context_read_from_file(context.get(), argv[1], nullptr));
    heif_item_id primary = 0;
    check(heif_context_get_primary_image_ID(context.get(), &primary));
    heif_image_handle *raw_handle = nullptr;
    check(heif_context_get_primary_image_handle(context.get(), &raw_handle));
    auto handle = std::unique_ptr<heif_image_handle, decltype(&heif_image_handle_release)>(
        raw_handle, heif_image_handle_release);
    auto options = std::unique_ptr<heif_decoding_options, decltype(&heif_decoding_options_free)>(
        heif_decoding_options_alloc(), heif_decoding_options_free);
    if (!options) {
      throw std::runtime_error("Cannot allocate decoder options");
    }
    options->strict_decoding = argc < 4 || std::string(argv[3]) != "recover";
    heif_image *raw_image = nullptr;
    check(heif_decode_image(handle.get(), &raw_image, heif_colorspace_RGB,
                           heif_chroma_interleaved_RGBA, options.get()));
    auto image = std::unique_ptr<heif_image, decltype(&heif_image_release)>(
        raw_image, heif_image_release);
    int warnings = heif_image_get_decoding_warnings(image.get(), 0, nullptr, 0);
    int width = heif_image_get_width(image.get(), heif_channel_interleaved);
    int height = heif_image_get_height(image.get(), heif_channel_interleaved);
    std::cout << "{\"primary_id\":" << primary << ",\"width\":" << width
              << ",\"height\":" << height << ",\"warnings\":" << warnings
              << ",\"strict\":" << (options->strict_decoding ? "true" : "false") << "}\n";
    if (argc >= 3 && std::string(argv[2]) != "-") {
      std::vector<uint8_t> icc(heif_image_get_raw_color_profile_size(image.get()));
      if (!icc.empty()) {
        check(heif_image_get_raw_color_profile(image.get(), icc.data()));
      }
      int stride = 0;
      const uint8_t *plane = heif_image_get_plane_readonly(image.get(), heif_channel_interleaved, &stride);
      FILE *file = std::fopen(argv[2], "wb");
      if (!file) {
        throw std::runtime_error("Cannot open PNG output");
      }
      png_structp png = png_create_write_struct(PNG_LIBPNG_VER_STRING, nullptr, nullptr, nullptr);
      if (!png) {
        std::fclose(file);
        throw std::runtime_error("Cannot allocate PNG writer");
      }
      png_infop info = png_create_info_struct(png);
      if (!info || setjmp(png_jmpbuf(png))) {
        png_destroy_write_struct(&png, info ? &info : nullptr);
        std::fclose(file);
        throw std::runtime_error("Cannot write PNG");
      }
      png_init_io(png, file);
      png_set_IHDR(png, info, width, height, 8, PNG_COLOR_TYPE_RGBA,
                   PNG_INTERLACE_NONE, PNG_COMPRESSION_TYPE_DEFAULT, PNG_FILTER_TYPE_DEFAULT);
      if (!icc.empty()) {
        png_set_iCCP(png, info, "ICC", PNG_COMPRESSION_TYPE_BASE, icc.data(), icc.size());
      }
      png_write_info(png, info);
      for (int y = 0; y < height; ++y) {
        png_write_row(png, plane + static_cast<size_t>(y) * stride);
      }
      png_write_end(png, info);
      png_destroy_write_struct(&png, &info);
      std::fclose(file);
    }
    return warnings == 0 ? 0 : 2;
  } catch (const std::exception &error) {
    std::cerr << error.what() << '\n';
    return 1;
  }
}
