using System;
using System.Windows;
using System.Windows.Threading;

namespace Timelens.Calibration
{
    public sealed class WpfFixture : Application
    {
        private Window _window;
        private DispatcherTimer _timer;
        private int _phase;

        [STAThread]
        public static void Main()
        {
            new WpfFixture { ShutdownMode = ShutdownMode.OnExplicitShutdown }.Run();
        }

        protected override void OnStartup(StartupEventArgs args)
        {
            base.OnStartup(args);
            _window = new Window
            {
                Title = "Timelens WPF fixture",
                Width = 520,
                Height = 320,
                Left = 220,
                Top = 180,
                Content = "Timelens WPF window-state calibration"
            };
            _window.Show();
            _window.Activate();

            _timer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(700) };
            _timer.Tick += OnTick;
            _timer.Start();
        }

        private void OnTick(object sender, EventArgs args)
        {
            _phase += 1;
            switch (_phase)
            {
                case 1:
                    _window.WindowState = WindowState.Minimized;
                    break;
                case 2:
                    _window.WindowState = WindowState.Normal;
                    break;
                case 3:
                    _window.Hide();
                    break;
                case 4:
                    _window.Show();
                    break;
                case 5:
                    _window.Close();
                    break;
                case 7:
                    _timer.Stop();
                    Shutdown();
                    break;
            }
        }
    }
}
