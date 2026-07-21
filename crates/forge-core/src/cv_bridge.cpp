#include <algorithm>
#include <cstdint>
#include <opencv2/core.hpp>
#include <opencv2/imgproc.hpp>
#include <vector>

extern "C" {
struct ForgeTemplate { const uint8_t *data; int32_t width; int32_t height; };
struct ForgeMatch { int32_t x; int32_t y; int32_t width; int32_t height; float confidence; int32_t template_index; };

void forge_cv_set_threads(int32_t threads){cv::setNumThreads(std::max(1,threads));}

int32_t forge_i420_to_rgb(const uint8_t *yuv,int32_t width,int32_t height,uint8_t *output){
  try{if(!yuv||!output||width<=0||height<=0)return -1;cv::Mat input(height+height/2,width,CV_8UC1,const_cast<uint8_t*>(yuv));cv::Mat rgb(height,width,CV_8UC3,output);cv::cvtColor(input,rgb,cv::COLOR_YUV2RGB_I420);return 0;}catch(const cv::Exception&){return -2;}
}

int32_t forge_match_template_rgb(
    const uint8_t *rgb_data, int32_t width, int32_t height, int32_t stride,
    int32_t channels,
    const ForgeTemplate *templates, int32_t template_count,
    int32_t x1, int32_t y1, int32_t x2, int32_t y2,
    float threshold, bool best_only, bool priority_first,
    int32_t coarse_candidates,
    ForgeMatch *output, int32_t capacity) {
  try {
    if (!rgb_data || !templates || !output || width <= 0 || height <= 0 ||
        x1 < 0 || y1 < 0 || x2 > width || y2 > height || x1 >= x2 || y1 >= y2)
      return -1;
    if(channels!=1&&channels!=3)return -1;
    cv::Mat rgb(height, width,channels==1?CV_8UC1:CV_8UC3,const_cast<uint8_t *>(rgb_data), stride);
    cv::Mat search = rgb(cv::Rect(x1, y1, x2 - x1, y2 - y1));
    constexpr int coarse_scale = 3;
    thread_local cv::Mat coarse_search,coarse_templ,coarse_scores,scores;
    if (best_only)
      cv::resize(search, coarse_search,
                 cv::Size(std::max(2, search.cols / coarse_scale),
                          std::max(2, search.rows / coarse_scale)),
                 0, 0, cv::INTER_AREA);
    std::vector<ForgeMatch> found;
    for (int32_t i = 0; i < template_count; ++i) {
      const auto &item = templates[i];
      if (!item.data || item.width >= search.cols || item.height >= search.rows) continue;
      cv::Mat templ(item.height,item.width,channels==1?CV_8UC1:CV_8UC3,const_cast<uint8_t *>(item.data));
      if (best_only) {
        cv::resize(templ, coarse_templ,
                   cv::Size(std::max(2, templ.cols / coarse_scale),
                            std::max(2, templ.rows / coarse_scale)),
                   0, 0, cv::INTER_AREA);
        if (coarse_templ.cols >= coarse_search.cols || coarse_templ.rows >= coarse_search.rows) continue;
        cv::matchTemplate(coarse_search, coarse_templ, coarse_scores, cv::TM_CCOEFF_NORMED);
        ForgeMatch best{};
        double best_score = -1.0;
        // 保留多个粗匹配峰值，避免小模板被错误的单一全局峰值淘汰。
        for (int candidate = 0; candidate < std::max(1,coarse_candidates); ++candidate) {
          double coarse_score;
          cv::Point coarse_point;
          cv::minMaxLoc(coarse_scores, nullptr, &coarse_score, nullptr, &coarse_point);
          const int approximate_x = coarse_point.x * coarse_scale;
          const int approximate_y = coarse_point.y * coarse_scale;
          const int margin = coarse_scale * 5;
          const int left = std::max(0, approximate_x - margin);
          const int top = std::max(0, approximate_y - margin);
          const int right = std::min(search.cols, approximate_x + item.width + margin);
          const int bottom = std::min(search.rows, approximate_y + item.height + margin);
          if (right-left>item.width && bottom-top>item.height) {
            cv::Mat local=search(cv::Rect(left,top,right-left,bottom-top));
            cv::matchTemplate(local,templ,scores,cv::TM_CCOEFF_NORMED);
            double score;cv::Point point;cv::minMaxLoc(scores,nullptr,&score,nullptr,&point);
            if(score>best_score){best_score=score;best={point.x+left+x1+item.width/2,point.y+top+y1+item.height/2,item.width,item.height,static_cast<float>(score),i};}
          }
          const int suppress_x=std::max(0,coarse_point.x-coarse_templ.cols/2);
          const int suppress_y=std::max(0,coarse_point.y-coarse_templ.rows/2);
          const int suppress_w=std::min(coarse_scores.cols-suppress_x,coarse_templ.cols);
          const int suppress_h=std::min(coarse_scores.rows-suppress_y,coarse_templ.rows);
          coarse_scores(cv::Rect(suppress_x,suppress_y,suppress_w,suppress_h)).setTo(-1.0f);
        }
        if (best_score >= threshold) {
          ForgeMatch match=best;
          if (priority_first) { output[0] = match; return 1; }
          found.push_back(match);
        }
      } else {
        cv::matchTemplate(search, templ, scores, cv::TM_CCOEFF_NORMED);
        // Extract local maxima with suppression instead of collecting every
        // overlapping pixel above threshold (which could allocate millions).
        while (static_cast<int32_t>(found.size()) < capacity) {
          double score; cv::Point point;
          cv::minMaxLoc(scores, nullptr, &score, nullptr, &point);
          if (score < threshold) break;
          found.push_back({point.x + x1 + item.width / 2, point.y + y1 + item.height / 2,
                           item.width, item.height, static_cast<float>(score), i});
          const int sx=std::max(0,point.x-item.width/2),sy=std::max(0,point.y-item.height/2);
          const int sw=std::min(scores.cols-sx,item.width),sh=std::min(scores.rows-sy,item.height);
          scores(cv::Rect(sx,sy,sw,sh)).setTo(-1.0f);
        }
      }
    }
    std::sort(found.begin(), found.end(), [](const auto &a, const auto &b) {
      return a.confidence > b.confidence;
    });
    int32_t count = std::min<int32_t>(capacity, found.size());
    std::copy_n(found.begin(), count, output);
    return count;
  } catch (const cv::Exception &) {
    return -2;
  }
}
}
