using System.Diagnostics;
using System.Text.Json;
using Microsoft.UI;
using Microsoft.UI.Dispatching;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using WinRT.Interop;
using Windows.Graphics;

namespace Timelens.WinUISpike;

public sealed class MainWindow : Window
{
    private readonly DispatcherQueueTimer _timer;
    private readonly Stopwatch _panStopwatch = new();
    private readonly List<Rectangle> _pool = [];
    private readonly Grid _root;
    private readonly TextBlock _statusText;
    private readonly Canvas _timelineCanvas;
    private List<TimelineSegment> _segments = [];
    private uint _step;
    private double _lastTickMilliseconds;
    private double _maxStepGapMilliseconds;
    private uint _lateStepCount;

    public MainWindow()
    {
        (_root, _statusText, _timelineCanvas) = BuildVisualTree();
        Content = _root;
        Title = "Timelens WinUI Spike";
        ResizeWindow();
        _root.Loaded += OnLoaded;
        _timer = DispatcherQueue.CreateTimer();
        _timer.Interval = TimeSpan.FromMilliseconds(16);
        _timer.Tick += OnPanTick;
    }

    private async void OnLoaded(object sender, RoutedEventArgs args)
    {
        try
        {
            var path = Environment.GetEnvironmentVariable("TIMELENS_SPIKE_TIMELINE")
                ?? throw new InvalidOperationException("TIMELENS_SPIKE_TIMELINE is missing");
            await using var stream = File.OpenRead(path);
            _segments = await JsonSerializer.DeserializeAsync<List<TimelineSegment>>(
                stream,
                new JsonSerializerOptions { PropertyNameCaseInsensitive = true }
            ) ?? throw new InvalidOperationException("timeline is empty");

            for (var index = 0; index < 512; index += 1)
            {
                var rectangle = new Rectangle
                {
                    Height = 28,
                    RadiusX = 3,
                    RadiusY = 3,
                    Visibility = Visibility.Collapsed,
                };
                _pool.Add(rectangle);
                _timelineCanvas.Children.Add(rectangle);
            }

            RenderVisible(0, 1);
            _panStopwatch.Start();
            _timer.Start();
        }
        catch (Exception exception)
        {
            _statusText.Text = exception.Message;
        }
    }

    private async void OnPanTick(DispatcherQueueTimer sender, object args)
    {
        var now = _panStopwatch.Elapsed.TotalMilliseconds;
        var gap = now - _lastTickMilliseconds;
        _lastTickMilliseconds = now;
        _maxStepGapMilliseconds = Math.Max(_maxStepGapMilliseconds, gap);
        if (gap > 25)
        {
            _lateStepCount += 1;
        }
        _step += 1;
        var panMinute = (_step * 53) % 42_000;
        var zoom = 0.8 + (_step % 40) * 0.015;
        RenderVisible(panMinute, zoom);
        _statusText.Text = $"Pan {panMinute} min · Zoom {Math.Round(zoom * 100)}%";

        if (_step != 240)
        {
            return;
        }

        _timer.Stop();
        _panStopwatch.Stop();
        var output = Environment.GetEnvironmentVariable("TIMELENS_SPIKE_OUTPUT")
            ?? throw new InvalidOperationException("TIMELENS_SPIKE_OUTPUT is missing");
        var payload = JsonSerializer.Serialize(new
        {
            steps = 240,
            elapsedMs = _panStopwatch.Elapsed.TotalMilliseconds,
            maxStepGapMs = _maxStepGapMilliseconds,
            lateStepCount = _lateStepCount,
        });
        await File.WriteAllTextAsync(System.IO.Path.Combine(output, "ui-pan.json"), payload);
    }

    private void RenderVisible(double panMinute, double zoom)
    {
        var right = panMinute + 1100 / zoom;
        var visibleIndex = 0;
        foreach (var segment in _segments)
        {
            var end = segment.StartMinute + segment.DurationMinutes;
            if (end < panMinute || segment.StartMinute > right)
            {
                continue;
            }
            if (visibleIndex == _pool.Count)
            {
                throw new InvalidOperationException("visible segment pool exhausted");
            }
            var rectangle = _pool[visibleIndex++];
            rectangle.Visibility = Visibility.Visible;
            rectangle.Width = Math.Max(1, segment.DurationMinutes * zoom);
            rectangle.Fill = new SolidColorBrush(segment.State switch
            {
                "displayed" => ColorHelper.FromArgb(255, 104, 167, 255),
                "focused" => ColorHelper.FromArgb(255, 108, 229, 161),
                _ => ColorHelper.FromArgb(255, 138, 124, 246),
            });
            Canvas.SetLeft(rectangle, (segment.StartMinute - panMinute) * zoom);
            Canvas.SetTop(rectangle, segment.Lane * 42 + 12);
        }
        for (; visibleIndex < _pool.Count; visibleIndex += 1)
        {
            _pool[visibleIndex].Visibility = Visibility.Collapsed;
        }
    }

    private void ResizeWindow()
    {
        var hwnd = WindowNative.GetWindowHandle(this);
        var id = Win32Interop.GetWindowIdFromWindow(hwnd);
        AppWindow.GetFromWindowId(id).Resize(new SizeInt32(1100, 680));
    }

    private static (Grid Root, TextBlock Status, Canvas Timeline) BuildVisualTree()
    {
        var root = new Grid
        {
            Background = new SolidColorBrush(ColorHelper.FromArgb(255, 17, 21, 28)),
        };
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
        root.RowDefinitions.Add(new RowDefinition { Height = new GridLength(1, GridUnitType.Star) });

        var heading = new TextBlock
        {
            Text = "WinUI 3 · 10,000 segment timeline",
            Margin = new Thickness(18, 18, 18, 0),
            Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 245, 247, 250)),
            FontSize = 24,
        };
        root.Children.Add(heading);

        var status = new TextBlock
        {
            Text = "Loading timeline…",
            Margin = new Thickness(18, 12, 18, 12),
            Foreground = new SolidColorBrush(ColorHelper.FromArgb(255, 152, 162, 179)),
            FontSize = 14,
        };
        Grid.SetRow(status, 1);
        root.Children.Add(status);

        var timeline = new Canvas
        {
            Margin = new Thickness(18, 0, 18, 18),
            Background = new SolidColorBrush(ColorHelper.FromArgb(255, 24, 30, 39)),
        };
        Grid.SetRow(timeline, 2);
        root.Children.Add(timeline);
        return (root, status, timeline);
    }

    private sealed record TimelineSegment(
        int Id,
        double StartMinute,
        double DurationMinutes,
        double Lane,
        string State
    );
}
