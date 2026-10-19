using System;
using System.IO;
using System.Reflection;
using System.Threading;

class UpdateFixture
{
    static void Main(string[] args)
    {
        if (args.Length == 2 && args[0] == "--wait-for")
        {
            while (!File.Exists(args[1])) Thread.Sleep(50);
            return;
        }
        if (args.Length == 2 && args[0] == "--data-dir")
        {
            Directory.CreateDirectory(args[1]);
            File.WriteAllText(Path.Combine(args[1], "restarted.txt"),
                Assembly.GetExecutingAssembly().GetName().Version.ToString());
        }
    }
}
